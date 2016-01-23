extern crate libc;

mod cursor;
#[doc(hidden)]
pub mod raw;

use std::cell::Cell;
use std::ffi::{CString, CStr};
use std::rc::Rc;
use std::ptr;

use backend::Pg;
use db_result::DbResult;
use expression::{AsExpression, NonAggregate};
use expression::expression_methods::*;
use helper_types::{FindBy, Limit};
use expression::helper_types::AsExpr;
use query_builder::{AsQuery, Query, QueryFragment};
use query_builder::pg::PgQueryBuilder;
use query_dsl::{FilterDsl, LimitDsl};
use query_source::{Table, Queryable};
use result::*;
use self::cursor::Cursor;
use self::raw::RawConnection;
use super::{SimpleConnection, Connection, PkType, FindPredicate};
use types::{NativeSqlType, ToSql};

/// The connection string expected by `PgConnection::establish`
/// should be a PostgreSQL connection string, as documented at
/// http://www.postgresql.org/docs/9.4/static/libpq-connect.html#LIBPQ-CONNSTRING
pub struct PgConnection {
    raw_connection: Rc<RawConnection>,
    transaction_depth: Cell<i32>,
}

unsafe impl Send for PgConnection {}

impl SimpleConnection for PgConnection {
    fn batch_execute(&self, query: &str) -> QueryResult<()> {
        let query = try!(CString::new(query));
        let inner_result = unsafe {
            self.raw_connection.exec(query.as_ptr())
        };
        try!(DbResult::new(self, inner_result));
        Ok(())
    }
}

impl Connection for PgConnection {
    type Backend = Pg;

    fn establish(database_url: &str) -> ConnectionResult<PgConnection> {
        RawConnection::establish(database_url).map(|raw_conn| {
            PgConnection {
                raw_connection: Rc::new(raw_conn),
                transaction_depth: Cell::new(0),
            }
        })
    }

    fn transaction<T, E, F>(&self, f: F) -> TransactionResult<T, E> where
        F: FnOnce() -> Result<T, E>,
    {
        try!(self.begin_transaction());
        match f() {
            Ok(value) => {
                try!(self.commit_transaction());
                Ok(value)
            },
            Err(e) => {
                try!(self.rollback_transaction());
                Err(TransactionError::UserReturnedError(e))
            },
        }
    }

    fn begin_test_transaction(&self) -> QueryResult<usize> {
        assert_eq!(self.transaction_depth.get(), 0);
        self.begin_transaction()
    }

    fn test_transaction<T, E, F>(&self, f: F) -> T where
        F: FnOnce() -> Result<T, E>,
    {
        let mut user_result = None;
        let _ = self.transaction::<(), _, _>(|| {
            user_result = f().ok();
            Err(())
        });
        user_result.expect("Transaction did not succeed")
    }

    fn execute(&self, query: &str) -> QueryResult<usize> {
        self.execute_inner(query).map(|res| res.rows_affected())
    }

    fn query_one<T, U>(&self, source: T) -> QueryResult<U> where
        T: AsQuery,
        T::Query: QueryFragment<Pg>,
        U: Queryable<T::SqlType>,
    {
        self.query_all(source)
            .and_then(|mut e| e.nth(0).map(Ok).unwrap_or(Err(Error::NotFound)))
    }

    fn query_all<'a, T, U: 'a>(&self, source: T) -> QueryResult<Box<Iterator<Item=U> + 'a>> where
        T: AsQuery,
        T::Query: QueryFragment<Pg>,
        U: Queryable<T::SqlType>,
    {
        let (sql, params, types) = self.prepare_query(&source.as_query());
        self.exec_sql_params(&sql, &params, &Some(types))
            .map(|r| Box::new(Cursor::new(r)) as Box<Iterator<Item=U>>)
    }

    fn find<T, U, PK>(&self, source: T, id: PK) -> QueryResult<U> where
        T: Table + FilterDsl<FindPredicate<T, PK>>,
        FindBy<T, T::PrimaryKey, PK>: LimitDsl,
        Limit<FindBy<T, T::PrimaryKey, PK>>: QueryFragment<Pg>,
        U: Queryable<<Limit<FindBy<T, T::PrimaryKey, PK>> as Query>::SqlType>,
        PK: AsExpression<PkType<T>>,
        AsExpr<PK, T::PrimaryKey>: NonAggregate,
    {
        let pk = source.primary_key();
        self.query_one(source.filter(pk.eq(id)).limit(1))
    }

    fn execute_returning_count<T>(&self, source: &T) -> QueryResult<usize> where
        T: QueryFragment<Pg>,
    {
        let (sql, params, param_types) = self.prepare_query(source);
        self.exec_sql_params(&sql, &params, &Some(param_types))
            .map(|r| r.rows_affected())
    }

    fn silence_notices<F: FnOnce() -> T, T>(&self, f: F) -> T {
        self.raw_connection.set_notice_processor(noop_notice_processor);
        let result = f();
        self.raw_connection.set_notice_processor(default_notice_processor);
        result
    }
}

impl PgConnection {
    fn exec_sql_params(&self, query: &str, param_data: &Vec<Option<Vec<u8>>>, param_types: &Option<Vec<u32>>) -> QueryResult<DbResult> {
        let query = try!(CString::new(query));
        let params_pointer = param_data.iter()
            .map(|data| data.as_ref().map(|d| d.as_ptr() as *const libc::c_char)
                 .unwrap_or(ptr::null()))
            .collect::<Vec<_>>();
        let param_types_ptr = param_types.as_ref()
            .map(|types| types.as_ptr())
            .unwrap_or(ptr::null());
        let param_lengths = param_data.iter()
            .map(|data| data.as_ref().map(|d| d.len() as libc::c_int)
                 .unwrap_or(0))
            .collect::<Vec<_>>();
        let param_formats = vec![1; param_data.len()];

        let internal_res = unsafe {
            self.raw_connection.exec_params(
                query.as_ptr(),
                params_pointer.len() as libc::c_int,
                param_types_ptr,
                params_pointer.as_ptr(),
                param_lengths.as_ptr(),
                param_formats.as_ptr(),
                1,
            )
        };

        DbResult::new(self, internal_res)
    }

    fn prepare_query<T: QueryFragment<Pg>>(&self, source: &T)
        -> (String, Vec<Option<Vec<u8>>>, Vec<u32>)
    {
        let mut query_builder = PgQueryBuilder::new(&self.raw_connection);
        source.to_sql(&mut query_builder).unwrap();
        (query_builder.sql, query_builder.binds, query_builder.bind_types)
    }

    fn execute_inner(&self, query: &str) -> QueryResult<DbResult> {
        self.exec_sql_params(query, &Vec::new(), &None)
    }

    #[doc(hidden)]
    pub fn last_error_message(&self) -> String {
        self.raw_connection.last_error_message()
    }

    fn begin_transaction(&self) -> QueryResult<usize> {
        let transaction_depth = self.transaction_depth.get();
        self.change_transaction_depth(1, if transaction_depth == 0 {
            self.execute("BEGIN")
        } else {
            self.execute(&format!("SAVEPOINT diesel_savepoint_{}", transaction_depth))
        })
    }

    fn rollback_transaction(&self) -> QueryResult<usize> {
        let transaction_depth = self.transaction_depth.get();
        self.change_transaction_depth(-1, if transaction_depth == 1 {
            self.execute("ROLLBACK")
        } else {
            self.execute(&format!("ROLLBACK TO SAVEPOINT diesel_savepoint_{}",
                                  transaction_depth - 1))
        })
    }

    fn commit_transaction(&self) -> QueryResult<usize> {
        let transaction_depth = self.transaction_depth.get();
        self.change_transaction_depth(-1, if transaction_depth <= 1 {
            self.execute("COMMIT")
        } else {
            self.execute(&format!("RELEASE SAVEPOINT diesel_savepoint_{}",
                                  transaction_depth - 1))
        })
    }

    fn change_transaction_depth(&self, by: i32, query: QueryResult<usize>) -> QueryResult<usize> {
        if query.is_ok() {
            self.transaction_depth.set(self.transaction_depth.get() + by);
        }
        query
    }
}

extern "C" fn noop_notice_processor(_: *mut libc::c_void, _message: *const libc::c_char) {
}

extern "C" fn default_notice_processor(_: *mut libc::c_void, message: *const libc::c_char) {
    use std::io::Write;
    let c_str = unsafe { CStr::from_ptr(message) };
    ::std::io::stderr().write(c_str.to_bytes()).unwrap();
}
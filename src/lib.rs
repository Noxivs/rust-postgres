/*!
Rust-Postgres is a pure-Rust frontend for the popular PostgreSQL database. It
exposes a high level interface in the vein of JDBC or Go's `database/sql`
package.

```rust,no_run
extern crate postgres;
extern crate time;

use time::Timespec;

use postgres::{PostgresConnection, NoSsl};
use postgres::types::ToSql;

struct Person {
    id: i32,
    name: ~str,
    time_created: Timespec,
    data: Option<Vec<u8>>
}

fn main() {
    let conn = PostgresConnection::connect("postgresql://postgres@localhost",
                                           &NoSsl).unwrap();

    conn.execute("CREATE TABLE person (
                    id              SERIAL PRIMARY KEY,
                    name            VARCHAR NOT NULL,
                    time_created    TIMESTAMP NOT NULL,
                    data            BYTEA
                  )", []).unwrap();
    let me = Person {
        id: 0,
        name: "Steven".to_owned(),
        time_created: time::get_time(),
        data: None
    };
    conn.execute("INSERT INTO person (name, time_created, data)
                    VALUES ($1, $2, $3)",
                 [&me.name as &ToSql, &me.time_created as &ToSql,
                  &me.data as &ToSql]).unwrap();

    let stmt = conn.prepare("SELECT id, name, time_created, data FROM person")
            .unwrap();
    for row in stmt.query([]).unwrap() {
        let person = Person {
            id: row[1],
            name: row[2],
            time_created: row[3],
            data: row[4]
        };
        println!("Found person {}", person.name);
    }
}
```
 */

#![crate_id="github.com/sfackler/rust-postgres#postgres:0.0"]
#![crate_type="rlib"]
#![crate_type="dylib"]
#![doc(html_root_url="http://sfackler.github.io/rust-postgres/doc")]

#![warn(missing_doc)]

#![feature(macro_rules, struct_variant, phase)]

extern crate collections;
extern crate openssl;
extern crate serialize;
extern crate sync;
extern crate time;
extern crate phf;
#[phase(syntax)]
extern crate phf_mac;
extern crate url;
#[phase(syntax, link)]
extern crate log;
extern crate uuid;

use collections::{Deque, HashMap, RingBuf};
use url::{UserInfo, Url};
use openssl::crypto::hash::{MD5, Hasher};
use openssl::ssl::SslContext;
use serialize::hex::ToHex;
use std::cell::{Cell, RefCell};
use std::from_str::FromStr;
use std::io::{BufferedStream, IoResult};
use std::io::net::ip::Port;
use std::mem;
use std::owned::Box;
use std::task;
use std::fmt;

use error::{InvalidUrl,
            MissingPassword,
            MissingUser,
            PgConnectDbError,
            PgConnectStreamError,
            PgDbError,
            PgInvalidColumn,
            PgStreamDesynchronized,
            PgStreamError,
            PgWrongParamCount,
            PostgresConnectError,
            PostgresDbError,
            PostgresError,
            UnsupportedAuthentication,
            PgWrongConnection};
use io::{MaybeSslStream,
         InternalStream};
use message::{AuthenticationCleartextPassword,
              AuthenticationGSS,
              AuthenticationKerberosV5,
              AuthenticationMD5Password,
              AuthenticationOk,
              AuthenticationSCMCredential,
              AuthenticationSSPI,
              BackendKeyData,
              BackendMessage,
              BindComplete,
              CommandComplete,
              DataRow,
              EmptyQueryResponse,
              ErrorResponse,
              NoData,
              NoticeResponse,
              NotificationResponse,
              ParameterDescription,
              ParameterStatus,
              ParseComplete,
              PortalSuspended,
              ReadyForQuery,
              RowDescription,
              RowDescriptionEntry};
use message::{Bind,
              CancelRequest,
              Close,
              Describe,
              Execute,
              FrontendMessage,
              Parse,
              PasswordMessage,
              Query,
              StartupMessage,
              Sync,
              Terminate};
use message::{WriteMessage, ReadMessage};
use types::{Oid, PostgresType, ToSql, FromSql, PgUnknownType};

macro_rules! try_pg_conn(
    ($e:expr) => (
        match $e {
            Ok(ok) => ok,
            Err(err) => return Err(PgConnectStreamError(err))
        }
    )
)

macro_rules! try_pg(
    ($e:expr) => (
        match $e {
            Ok(ok) => ok,
            Err(err) => return Err(PgStreamError(err))
        }
    )
)

macro_rules! try_desync(
    ($e:expr) => (
        match $e {
            Ok(ok) => ok,
            Err(err) => {
                self.desynchronized = true;
                return Err(err);
            }
        }
    )
)

macro_rules! check_desync(
    ($e:expr) => ({
        if $e.canary() != CANARY {
            fail!("PostgresConnection use after free. See mozilla/rust#13246.");
        }
        if $e.is_desynchronized() {
            return Err(PgStreamDesynchronized);
        }
    })
)

pub mod error;
mod io;
pub mod pool;
mod message;
pub mod types;
#[cfg(test)]
mod test;

static CANARY: u32 = 0xdeadbeef;

/// A typedef of the result returned by many methods.
pub type PostgresResult<T> = Result<T, PostgresError>;

/// Specifies the target server to connect to.
#[deriving(Clone)]
pub enum PostgresConnectTarget {
    /// Connect via TCP to the specified host.
    TargetTcp(~str),
    /// Connect via a Unix domain socket in the specified directory.
    TargetUnix(Path)
}

/// Information necessary to open a new connection to a Postgres server.
#[deriving(Clone)]
pub struct PostgresConnectParams {
    /// The target server
    pub target: PostgresConnectTarget,
    /// The target port.
    ///
    /// Defaults to 5432 if not specified.
    pub port: Option<Port>,
    /// The user to login as.
    ///
    /// `PostgresConnection::connect` requires a user but `cancel_query` does
    /// not.
    pub user: Option<~str>,
    /// An optional password used for authentication
    pub password: Option<~str>,
    /// The database to connect to. Defaults the value of `user`.
    pub database: Option<~str>,
    /// Runtime parameters to be passed to the Postgres backend.
    pub options: Vec<(~str, ~str)>,
}

/// A trait implemented by types that can be converted into a
/// `PostgresConnectParams`.
pub trait IntoConnectParams {
    /// Converts the value of `self` into a `PostgresConnectParams`.
    fn into_connect_params(self) -> Result<PostgresConnectParams,
                                           PostgresConnectError>;
}

impl IntoConnectParams for PostgresConnectParams {
    fn into_connect_params(self) -> Result<PostgresConnectParams,
                                           PostgresConnectError> {
        Ok(self)
    }
}

impl<'a> IntoConnectParams for &'a str {
    fn into_connect_params(self) -> Result<PostgresConnectParams,
                                           PostgresConnectError> {
        let Url {
            host,
            port,
            user,
            path,
            query: options,
            ..
        } = match url::from_str(self) {
            Ok(url) => url,
            Err(err) => return Err(InvalidUrl(err))
        };

        let maybe_path = url::decode_component(host);
        let target = if maybe_path.starts_with("/") {
            TargetUnix(Path::new(maybe_path))
        } else {
            TargetTcp(host)
        };

        let (user, pass) = match user {
            Some(UserInfo { user, pass }) => (Some(user), pass),
            None => (None, None),
        };

        let port = match port {
            Some(port) => match FromStr::from_str(port) {
                Some(port) => Some(port),
                None => return Err(InvalidUrl("invalid port".to_owned())),
            },
            None => None,
        };

        let database = if !path.is_empty() {
            // path contains the leading /
            let (_, path) = path.slice_shift_char();
            Some(path.to_owned())
        } else {
            None
        };

        Ok(PostgresConnectParams {
            target: target,
            port: port,
            user: user,
            password: pass,
            database: database,
            options: options,
        })
    }
}

/// Trait for types that can handle Postgres notice messages
pub trait PostgresNoticeHandler {
    /// Handle a Postgres notice message
    fn handle(&mut self, notice: PostgresDbError);
}

/// A notice handler which logs at the `info` level.
///
/// This is the default handler used by a `PostgresConnection`.
pub struct DefaultNoticeHandler;

impl PostgresNoticeHandler for DefaultNoticeHandler {
    fn handle(&mut self, notice: PostgresDbError) {
        info!("{}: {}", notice.severity, notice.message);
    }
}

/// An asynchronous notification
pub struct PostgresNotification {
    /// The process ID of the notifying backend process
    pub pid: i32,
    /// The name of the channel that the notify has been raised on
    pub channel: ~str,
    /// The "payload" string passed from the notifying process
    pub payload: ~str,
}

/// An iterator over asynchronous notifications
pub struct PostgresNotifications<'conn> {
    conn: &'conn PostgresConnection
}

impl<'conn> Iterator<PostgresNotification> for PostgresNotifications<'conn> {
    /// Returns the oldest pending notification or `None` if there are none.
    ///
    /// # Note
    ///
    /// `next` may return `Some` notification after returning `None` if a new
    /// notification was received.
    fn next(&mut self) -> Option<PostgresNotification> {
        self.conn.conn.borrow_mut().notifications.pop_front()
    }
}

/// Contains information necessary to cancel queries for a session
pub struct PostgresCancelData {
    /// The process ID of the session
    pub process_id: i32,
    /// The secret key for the session
    pub secret_key: i32,
}

/// Attempts to cancel an in-progress query.
///
/// The backend provides no information about whether a cancellation attempt
/// was successful or not. An error will only be returned if the driver was
/// unable to connect to the database.
///
/// A `PostgresCancelData` object can be created via
/// `PostgresConnection::cancel_data`. The object can cancel any query made on
/// that connection.
///
/// Only the host and port of the connetion info are used. See
/// `PostgresConnection::connect` for details of the `params` argument.
///
/// # Example
///
/// ```rust,no_run
/// # use postgres::{PostgresConnection, NoSsl};
/// # let url = "";
/// let conn = PostgresConnection::connect(url, &NoSsl).unwrap();
/// let cancel_data = conn.cancel_data();
/// spawn(proc() {
///     conn.execute("SOME EXPENSIVE QUERY", []).unwrap();
/// });
/// # let _ =
/// postgres::cancel_query(url, &NoSsl, cancel_data);
/// ```
pub fn cancel_query<T: IntoConnectParams>(params: T, ssl: &SslMode,
                                          data: PostgresCancelData)
                                          -> Result<(), PostgresConnectError> {
    let params = try!(params.into_connect_params());

    let mut socket = match io::initialize_stream(&params, ssl) {
        Ok(socket) => socket,
        Err(err) => return Err(err)
    };

    try_pg_conn!(socket.write_message(&CancelRequest {
        code: message::CANCEL_CODE,
        process_id: data.process_id,
        secret_key: data.secret_key
    }));
    try_pg_conn!(socket.flush());

    Ok(())
}

struct InnerPostgresConnection {
    stream: BufferedStream<MaybeSslStream<InternalStream>>,
    next_stmt_id: uint,
    notice_handler: Box<PostgresNoticeHandler:Send>,
    notifications: RingBuf<PostgresNotification>,
    cancel_data: PostgresCancelData,
    unknown_types: HashMap<Oid, ~str>,
    desynchronized: bool,
    finished: bool,
    canary: u32,
}

impl Drop for InnerPostgresConnection {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish_inner();
        }
    }
}

impl InnerPostgresConnection {
    fn connect<T: IntoConnectParams>(params: T, ssl: &SslMode)
                                     -> Result<InnerPostgresConnection,
                                               PostgresConnectError> {
        let params = try!(params.into_connect_params());
        let stream = try!(io::initialize_stream(&params, ssl));

        let mut conn = InnerPostgresConnection {
            stream: BufferedStream::new(stream),
            next_stmt_id: 0,
            notice_handler: box DefaultNoticeHandler,
            notifications: RingBuf::new(),
            cancel_data: PostgresCancelData { process_id: 0, secret_key: 0 },
            unknown_types: HashMap::new(),
            desynchronized: false,
            finished: false,
            canary: CANARY,
        };

        let PostgresConnectParams {
            user,
            password,
            database,
            mut options,
            ..
        } = params;

        let user = match user {
            Some(user) => user,
            None => return Err(MissingUser),
        };

        options.push(("client_encoding".to_owned(), "UTF8".to_owned()));
        // Postgres uses the value of TimeZone as the time zone for TIMESTAMP
        // WITH TIME ZONE values. Timespec converts to GMT internally.
        options.push(("TimeZone".to_owned(), "GMT".to_owned()));
        // We have to clone here since we need the user again for auth
        options.push(("user".to_owned(), user.clone()));
        match database {
            Some(database) => options.push(("database".to_owned(), database)),
            None => {}
        }

        try_pg_conn!(conn.write_messages([StartupMessage {
            version: message::PROTOCOL_VERSION,
            parameters: options.as_slice()
        }]));

        try!(conn.handle_auth(user, password));

        loop {
            match try_pg_conn!(conn.read_message()) {
                BackendKeyData { process_id, secret_key } => {
                    conn.cancel_data.process_id = process_id;
                    conn.cancel_data.secret_key = secret_key;
                }
                ReadyForQuery { .. } => break,
                ErrorResponse { fields } =>
                    return Err(PgConnectDbError(PostgresDbError::new(fields))),
                _ => unreachable!()
            }
        }

        Ok(conn)
    }

    fn write_messages(&mut self, messages: &[FrontendMessage])
            -> IoResult<()> {
        assert!(!self.desynchronized);
        for message in messages.iter() {
            try_desync!(self.stream.write_message(message));
        }
        Ok(try_desync!(self.stream.flush()))
    }

    fn read_message(&mut self) -> IoResult<BackendMessage> {
        assert!(!self.desynchronized);
        loop {
            match try_desync!(self.stream.read_message()) {
                NoticeResponse { fields } =>
                    self.notice_handler.handle(PostgresDbError::new(fields)),
                NotificationResponse { pid, channel, payload } =>
                    self.notifications.push_back(PostgresNotification {
                        pid: pid,
                        channel: channel,
                        payload: payload
                    }),
                ParameterStatus { parameter, value } =>
                    debug!("Parameter {} = {}", parameter, value),
                val => return Ok(val)
            }
        }
    }

    fn handle_auth(&mut self, user: ~str, pass: Option<~str>)
            -> Result<(), PostgresConnectError> {
        match try_pg_conn!(self.read_message()) {
            AuthenticationOk => return Ok(()),
            AuthenticationCleartextPassword => {
                let pass = match pass {
                    Some(pass) => pass,
                    None => return Err(MissingPassword)
                };
                try_pg_conn!(self.write_messages([PasswordMessage {
                        password: pass
                    }]));
            }
            AuthenticationMD5Password { salt } => {
                let pass = match pass {
                    Some(pass) => pass,
                    None => return Err(MissingPassword)
                };
                let hasher = Hasher::new(MD5);
                hasher.update(pass.as_bytes());
                hasher.update(user.as_bytes());
                let output = hasher.final().as_slice().to_hex();
                let hasher = Hasher::new(MD5);
                hasher.update(output.as_bytes());
                hasher.update(salt);
                let output = "md5" + hasher.final().as_slice().to_hex();
                try_pg_conn!(self.write_messages([PasswordMessage {
                        password: output.as_slice()
                    }]));
            }
            AuthenticationKerberosV5
            | AuthenticationSCMCredential
            | AuthenticationGSS
            | AuthenticationSSPI => return Err(UnsupportedAuthentication),
            ErrorResponse { fields } =>
                return Err(PgConnectDbError(PostgresDbError::new(fields))),
            _ => unreachable!()
        }

        match try_pg_conn!(self.read_message()) {
            AuthenticationOk => Ok(()),
            ErrorResponse { fields } =>
                Err(PgConnectDbError(PostgresDbError::new(fields))),
            _ => unreachable!()
        }
    }

    fn set_notice_handler(&mut self, handler: Box<PostgresNoticeHandler:Send>)
            -> Box<PostgresNoticeHandler:Send> {
        mem::replace(&mut self.notice_handler, handler)
    }

    fn prepare<'a>(&mut self, query: &str, conn: &'a PostgresConnection)
            -> PostgresResult<PostgresStatement<'a>> {
        let stmt_name = format!("s{}", self.next_stmt_id);
        self.next_stmt_id += 1;

        try_pg!(self.write_messages([
            Parse {
                name: stmt_name,
                query: query,
                param_types: []
            },
            Describe {
                variant: 'S' as u8,
                name: stmt_name
            },
            Sync]));

        match try_pg!(self.read_message()) {
            ParseComplete => {}
            ErrorResponse { fields } => {
                try!(self.wait_for_ready());
                return Err(PgDbError(PostgresDbError::new(fields)));
            }
            _ => unreachable!()
        }

        let mut param_types: Vec<PostgresType> = match try_pg!(self.read_message()) {
            ParameterDescription { types } =>
                types.iter().map(|ty| PostgresType::from_oid(*ty)).collect(),
            _ => unreachable!()
        };

        let mut result_desc: Vec<ResultDescription> = match try_pg!(self.read_message()) {
            RowDescription { descriptions } =>
                descriptions.move_iter().map(|desc| {
                    let RowDescriptionEntry { name, type_oid, .. } = desc;
                    ResultDescription {
                        name: name,
                        ty: PostgresType::from_oid(type_oid)
                    }
                }).collect(),
            NoData => Vec::new(),
            _ => unreachable!()
        };

        try!(self.wait_for_ready());

        // now that the connection is ready again, get unknown type names
        try!(self.set_type_names(param_types.mut_iter()));
        try!(self.set_type_names(result_desc.mut_iter().map(|d| &mut d.ty)));

        Ok(PostgresStatement {
            conn: conn,
            name: stmt_name,
            param_types: param_types,
            result_desc: result_desc,
            next_portal_id: Cell::new(0),
            finished: false,
        })
    }

    fn set_type_names<'a, I: Iterator<&'a mut PostgresType>>(&mut self, mut it: I)
            -> PostgresResult<()> {
        for ty in it {
            match *ty {
                PgUnknownType { oid, ref mut name } =>
                    *name = try!(self.get_type_name(oid)),
                _ => {}
            }
        }
        Ok(())
    }

    fn get_type_name(&mut self, oid: Oid) -> PostgresResult<~str> {
        match self.unknown_types.find(&oid) {
            Some(name) => return Ok(name.clone()),
            None => {}
        }
        let name = try!(self.quick_query(format!("SELECT typname FROM pg_type WHERE oid={}", oid)))
            .move_iter().next().unwrap().move_iter().next().unwrap().unwrap();
        self.unknown_types.insert(oid, name.clone());
        Ok(name)
    }

    fn is_desynchronized(&self) -> bool {
        self.desynchronized
    }

    fn canary(&self) -> u32 {
        self.canary
    }

    fn wait_for_ready(&mut self) -> PostgresResult<()> {
        match try_pg!(self.read_message()) {
            ReadyForQuery { .. } => Ok(()),
            _ => unreachable!()
        }
    }

    fn quick_query(&mut self, query: &str)
            -> PostgresResult<Vec<Vec<Option<~str>>>> {
        check_desync!(self);
        try_pg!(self.write_messages([Query { query: query }]));

        let mut result = Vec::new();
        loop {
            match try_pg!(self.read_message()) {
                ReadyForQuery { .. } => break,
                DataRow { row } => {
                    result.push(row.move_iter().map(|opt| {
                            opt.map(|b| StrBuf::from_utf8(b).unwrap().into_owned())
                        }).collect());
                }
                ErrorResponse { fields } => {
                    try!(self.wait_for_ready());
                    return Err(PgDbError(PostgresDbError::new(fields)));
                }
                _ => {}
            }
        }
        Ok(result)
    }

    fn finish_inner(&mut self) -> PostgresResult<()> {
        check_desync!(self);
        self.canary = 0;
        try_pg!(self.write_messages([Terminate]));
        Ok(())
    }
}

/// A connection to a Postgres database.
pub struct PostgresConnection {
    conn: RefCell<InnerPostgresConnection>
}

impl PostgresConnection {
    /// Creates a new connection to a Postgres database.
    ///
    /// Most applications can use a URL string in the normal format:
    ///
    /// ```notrust
    /// postgresql://user[:password]@host[:port][/database][?param1=val1[[&param2=val2]...]]
    /// ```
    ///
    /// The password may be omitted if not required. The default Postgres port
    /// (5432) is used if none is specified. The database name defaults to the
    /// username if not specified.
    ///
    /// To connect to the server via Unix sockets, `host` should be set to the
    /// absolute path of the directory containing the socket file. Since `/` is
    /// a reserved character in URLs, the path should be URL encoded.  If the
    /// path contains non-UTF 8 characters, a `PostgresConnectParams` struct
    /// should be created manually and passed in. Note that Postgres does not
    /// support SSL over Unix sockets.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use postgres::{PostgresConnection, NoSsl};
    /// let url = "postgresql://postgres:hunter2@localhost:2994/foodb";
    /// let maybe_conn = PostgresConnection::connect(url, &NoSsl);
    /// let conn = match maybe_conn {
    ///     Ok(conn) => conn,
    ///     Err(err) => fail!("Error connecting: {}", err)
    /// };
    /// ```
    ///
    /// ```rust,no_run
    /// # use postgres::{PostgresConnection, NoSsl};
    /// let url = "postgresql://postgres@%2Frun%2Fpostgres";
    /// let maybe_conn = PostgresConnection::connect(url, &NoSsl);
    /// ```
    ///
    /// ```rust,no_run
    /// # use postgres::{PostgresConnection, PostgresConnectParams, NoSsl, TargetUnix};
    /// # let some_crazy_path = Path::new("");
    /// let params = PostgresConnectParams {
    ///     target: TargetUnix(some_crazy_path),
    ///     port: None,
    ///     user: Some("postgres".to_owned()),
    ///     password: None,
    ///     database: None,
    ///     options: Vec::new(),
    /// };
    /// let maybe_conn = PostgresConnection::connect(params, &NoSsl);
    /// ```
    pub fn connect<T: IntoConnectParams>(params: T, ssl: &SslMode)
                                         -> Result<PostgresConnection,
                                                   PostgresConnectError> {
        InnerPostgresConnection::connect(params, ssl).map(|conn| {
            PostgresConnection { conn: RefCell::new(conn) }
        })
    }

    /// Sets the notice handler for the connection, returning the old handler.
    pub fn set_notice_handler(&self, handler: Box<PostgresNoticeHandler:Send>)
            -> Box<PostgresNoticeHandler:Send> {
        self.conn.borrow_mut().set_notice_handler(handler)
    }

    /// Returns an iterator over asynchronous notification messages.
    ///
    /// Use the `LISTEN` command to register this connection for notifications.
    pub fn notifications<'a>(&'a self) -> PostgresNotifications<'a> {
        PostgresNotifications {
            conn: self
        }
    }

    /// Creates a new prepared statement.
    ///
    /// A statement may contain parameters, specified by `$n` where `n` is the
    /// index of the parameter in the list provided at execution time,
    /// 1-indexed.
    ///
    /// The statement is associated with the connection that created it and may
    /// not outlive that connection.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use postgres::{PostgresConnection, NoSsl};
    /// # let conn = PostgresConnection::connect("", &NoSsl).unwrap();
    /// let maybe_stmt = conn.prepare("SELECT foo FROM bar WHERE baz = $1");
    /// let stmt = match maybe_stmt {
    ///     Ok(stmt) => stmt,
    ///     Err(err) => fail!("Error preparing statement: {}", err)
    /// };
    pub fn prepare<'a>(&'a self, query: &str)
            -> PostgresResult<PostgresStatement<'a>> {
        self.conn.borrow_mut().prepare(query, self)
    }

    /// Begins a new transaction.
    ///
    /// Returns a `PostgresTransaction` object which should be used instead of
    /// the connection for the duration of the transaction. The transaction
    /// is active until the `PostgresTransaction` object falls out of scope.
    /// A transaction will commit by default unless the task fails or the
    /// transaction is set to roll back.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use postgres::{PostgresConnection, NoSsl};
    /// # fn foo() -> Result<(), postgres::error::PostgresError> {
    /// # let conn = PostgresConnection::connect("", &NoSsl).unwrap();
    /// let trans = try!(conn.transaction());
    /// try!(trans.execute("UPDATE foo SET bar = 10", []));
    ///
    /// # let something_bad_happened = true;
    /// if something_bad_happened {
    ///     trans.set_rollback();
    /// }
    ///
    /// drop(trans);
    /// # Ok(())
    /// # }
    /// ```
    pub fn transaction<'a>(&'a self)
            -> PostgresResult<PostgresTransaction<'a>> {
        check_desync!(self);
        try!(self.quick_query("BEGIN"));
        Ok(PostgresTransaction {
            conn: self,
            commit: Cell::new(true),
            nested: false,
            finished: false,
        })
    }

    /// A convenience function for queries that are only run once.
    ///
    /// If an error is returned, it could have come from either the preparation
    /// or execution of the statement.
    ///
    /// On success, returns the number of rows modified or 0 if not applicable.
    pub fn execute(&self, query: &str, params: &[&ToSql])
            -> PostgresResult<uint> {
        self.prepare(query).and_then(|stmt| stmt.execute(params))
    }

    /// Returns information used to cancel pending queries.
    ///
    /// Used with the `cancel_query` function. The object returned can be used
    /// to cancel any query executed by the connection it was created from.
    pub fn cancel_data(&self) -> PostgresCancelData {
        self.conn.borrow().cancel_data
    }

    /// Returns whether or not the stream has been desynchronized due to an
    /// error in the communication channel with the server.
    ///
    /// If this has occurred, all further queries will immediately return an
    /// error.
    pub fn is_desynchronized(&self) -> bool {
        self.conn.borrow().is_desynchronized()
    }

    /// Consumes the connection, closing it.
    ///
    /// Functionally equivalent to the `Drop` implementation for
    /// `PostgresConnection` except that it returns any error encountered to
    /// the caller.
    pub fn finish(self) -> PostgresResult<()> {
        let mut conn = self.conn.borrow_mut();
        conn.finished = true;
        conn.finish_inner()
    }

    fn canary(&self) -> u32 {
        self.conn.borrow().canary()
    }

    fn quick_query(&self, query: &str)
            -> PostgresResult<Vec<Vec<Option<~str>>>> {
        self.conn.borrow_mut().quick_query(query)
    }

    fn wait_for_ready(&self) -> PostgresResult<()> {
        self.conn.borrow_mut().wait_for_ready()
    }

    fn read_message(&self) -> IoResult<BackendMessage> {
        self.conn.borrow_mut().read_message()
    }

    fn write_messages(&self, messages: &[FrontendMessage]) -> IoResult<()> {
        self.conn.borrow_mut().write_messages(messages)
    }
}

/// Specifies the SSL support requested for a new connection
pub enum SslMode {
    /// The connection will not use SSL
    NoSsl,
    /// The connection will use SSL if the backend supports it
    PreferSsl(SslContext),
    /// The connection must use SSL
    RequireSsl(SslContext)
}

/// Represents a transaction on a database connection
pub struct PostgresTransaction<'conn> {
    conn: &'conn PostgresConnection,
    commit: Cell<bool>,
    nested: bool,
    finished: bool,
}

#[unsafe_destructor]
impl<'conn> Drop for PostgresTransaction<'conn> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish_inner();
        }
    }
}

impl<'conn> PostgresTransaction<'conn> {
    fn finish_inner(&mut self) -> PostgresResult<()> {
        if task::failing() || !self.commit.get() {
            if self.nested {
                try!(self.conn.quick_query("ROLLBACK TO sp"));
            } else {
                try!(self.conn.quick_query("ROLLBACK"));
            }
        } else {
            if self.nested {
                try!(self.conn.quick_query("RELEASE sp"));
            } else {
                try!(self.conn.quick_query("COMMIT"));
            }
        }
        Ok(())
    }

    /// Like `PostgresConnection::prepare`.
    pub fn prepare<'a>(&'a self, query: &str)
            -> PostgresResult<PostgresStatement<'a>> {
        self.conn.prepare(query)
    }

    /// Like `PostgresConnection::execute`.
    pub fn execute(&self, query: &str, params: &[&ToSql])
            -> PostgresResult<uint> {
        self.conn.execute(query, params)
    }

    /// Like `PostgresConnection::transaction`.
    pub fn transaction<'a>(&'a self)
            -> PostgresResult<PostgresTransaction<'a>> {
        check_desync!(self.conn);
        try!(self.conn.quick_query("SAVEPOINT sp"));
        Ok(PostgresTransaction {
            conn: self.conn,
            commit: Cell::new(true),
            nested: true,
            finished: false,
        })
    }

    /// Like `PostgresConnection::notifications`.
    pub fn notifications<'a>(&'a self) -> PostgresNotifications<'a> {
        self.conn.notifications()
    }

    /// Like `PostgresConnection::is_desynchronized`.
    pub fn is_desynchronized(&self) -> bool {
        self.conn.is_desynchronized()
    }

    /// Determines if the transaction is currently set to commit or roll back.
    pub fn will_commit(&self) -> bool {
        self.commit.get()
    }

    /// Sets the transaction to commit at its completion.
    pub fn set_commit(&self) {
        self.commit.set(true);
    }

    /// Sets the transaction to roll back at its completion.
    pub fn set_rollback(&self) {
        self.commit.set(false);
    }

    /// Consumes the transaction, commiting or rolling it back as appropriate.
    ///
    /// Functionally equivalent to the `Drop` implementation of
    /// `PostgresTransaction` except that it returns any error to the caller.
    pub fn finish(mut self) -> PostgresResult<()> {
        self.finished = true;
        self.finish_inner()
    }

    /// Executes a prepared statement, returning a lazily loaded iterator over
    /// the resulting rows.
    ///
    /// No more than `row_limit` rows will be stored in memory at a time. Rows
    /// will be pulled from the database in batches of `row_limit` as needed.
    /// If `row_limit` is 0, `lazy_query` is equivalent to `query`.
    pub fn lazy_query<'trans, 'stmt>(&'trans self,
                                     stmt: &'stmt PostgresStatement,
                                     params: &[&ToSql],
                                     row_limit: uint)
                                     -> PostgresResult<PostgresLazyRows
                                                       <'trans, 'stmt>> {
        if self.conn as *_ != stmt.conn as *_ {
            return Err(PgWrongConnection);
        }
        check_desync!(self.conn);
        stmt.lazy_query(row_limit, params).map(|result| {
            PostgresLazyRows {
                trans: self,
                result: result
            }
        })
    }
}

/// A prepared statement
pub struct PostgresStatement<'conn> {
    conn: &'conn PostgresConnection,
    name: ~str,
    param_types: Vec<PostgresType>,
    result_desc: Vec<ResultDescription>,
    next_portal_id: Cell<uint>,
    finished: bool,
}

#[unsafe_destructor]
impl<'conn> Drop for PostgresStatement<'conn> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish_inner();
        }
    }
}

impl<'conn> PostgresStatement<'conn> {
    fn finish_inner(&mut self) -> PostgresResult<()> {
        check_desync!(self.conn);
        try_pg!(self.conn.write_messages([
            Close {
                variant: 'S' as u8,
                name: self.name.as_slice()
            },
            Sync]));
        loop {
            match try_pg!(self.conn.read_message()) {
                ReadyForQuery { .. } => break,
                ErrorResponse { fields } => {
                    try!(self.conn.wait_for_ready());
                    return Err(PgDbError(PostgresDbError::new(fields)));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn inner_execute(&self, portal_name: &str, row_limit: uint, params: &[&ToSql])
            -> PostgresResult<()> {
        if self.param_types.len() != params.len() {
            return Err(PgWrongParamCount {
                expected: self.param_types.len(),
                actual: params.len(),
            });
        }
        let mut formats = Vec::new();
        let mut values = Vec::new();
        for (&param, ty) in params.iter().zip(self.param_types.iter()) {
            let (format, value) = try!(param.to_sql(ty));
            formats.push(format as i16);
            values.push(value);
        };

        let result_formats: Vec<i16> = self.result_desc.iter().map(|desc| {
            desc.ty.result_format() as i16
        }).collect();

        try_pg!(self.conn.write_messages([
            Bind {
                portal: portal_name,
                statement: self.name.as_slice(),
                formats: formats.as_slice(),
                values: values.as_slice(),
                result_formats: result_formats.as_slice()
            },
            Execute {
                portal: portal_name,
                max_rows: row_limit as i32
            },
            Sync]));

        match try_pg!(self.conn.read_message()) {
            BindComplete => Ok(()),
            ErrorResponse { fields } => {
                try!(self.conn.wait_for_ready());
                Err(PgDbError(PostgresDbError::new(fields)))
            }
            _ => unreachable!()
        }
    }

    fn lazy_query<'a>(&'a self, row_limit: uint, params: &[&ToSql])
            -> PostgresResult<PostgresRows<'a>> {
        let id = self.next_portal_id.get();
        self.next_portal_id.set(id + 1);
        let portal_name = format!("{}p{}", self.name, id);

        try!(self.inner_execute(portal_name, row_limit, params));

        let mut result = PostgresRows {
            stmt: self,
            name: portal_name,
            data: RingBuf::new(),
            row_limit: row_limit,
            more_rows: true,
            finished: false,
        };
        try!(result.read_rows())

        Ok(result)
    }

    /// Returns a slice containing the expected parameter types.
    pub fn param_types<'a>(&'a self) -> &'a [PostgresType] {
        self.param_types.as_slice()
    }

    /// Returns a slice describing the columns of the result of the query.
    pub fn result_descriptions<'a>(&'a self) -> &'a [ResultDescription] {
        self.result_desc.as_slice()
    }

    /// Executes the prepared statement, returning the number of rows modified.
    ///
    /// If the statement does not modify any rows (e.g. SELECT), 0 is returned.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use postgres::{PostgresConnection, NoSsl};
    /// # use postgres::types::ToSql;
    /// # let conn = PostgresConnection::connect("", &NoSsl).unwrap();
    /// # let bar = 1i32;
    /// # let baz = true;
    /// let stmt = conn.prepare("UPDATE foo SET bar = $1 WHERE baz = $2").unwrap();
    /// match stmt.execute([&bar as &ToSql, &baz as &ToSql]) {
    ///     Ok(count) => println!("{} row(s) updated", count),
    ///     Err(err) => println!("Error executing query: {}", err)
    /// }
    pub fn execute(&self, params: &[&ToSql]) -> PostgresResult<uint> {
        check_desync!(self.conn);
        try!(self.inner_execute("", 0, params));

        let num;
        loop {
            match try_pg!(self.conn.read_message()) {
                DataRow { .. } => {}
                ErrorResponse { fields } => {
                    try!(self.conn.wait_for_ready());
                    return Err(PgDbError(PostgresDbError::new(fields)));
                }
                CommandComplete { tag } => {
                    let s = tag.split(' ').last().unwrap();
                    num = match FromStr::from_str(s) {
                        None => 0,
                        Some(n) => n
                    };
                    break;
                }
                EmptyQueryResponse => {
                    num = 0;
                    break;
                }
                _ => unreachable!()
            }
        }
        try!(self.conn.wait_for_ready());

        Ok(num)
    }

    /// Executes the prepared statement, returning an iterator over the
    /// resulting rows.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use postgres::{PostgresConnection, NoSsl};
    /// # use postgres::types::ToSql;
    /// # let conn = PostgresConnection::connect("", &NoSsl).unwrap();
    /// let stmt = conn.prepare("SELECT foo FROM bar WHERE baz = $1").unwrap();
    /// # let baz = true;
    /// let mut rows = match stmt.query([&baz as &ToSql]) {
    ///     Ok(rows) => rows,
    ///     Err(err) => fail!("Error running query: {}", err)
    /// };
    /// for row in rows {
    ///     let foo: i32 = row["foo"];
    ///     println!("foo: {}", foo);
    /// }
    /// ```
    pub fn query<'a>(&'a self, params: &[&ToSql])
            -> PostgresResult<PostgresRows<'a>> {
        check_desync!(self.conn);
        self.lazy_query(0, params)
    }

    /// Consumes the statement, clearing it from the Postgres session.
    ///
    /// Functionally identical to the `Drop` implementation of the
    /// `PostgresStatement` except that it returns any error to the caller.
    pub fn finish(mut self) -> PostgresResult<()> {
        self.finished = true;
        self.finish_inner()
    }
}

/// Information about a column of the result of a query.
#[deriving(Eq)]
pub struct ResultDescription {
    /// The name of the column
    pub name: ~str,
    /// The type of the data in the column
    pub ty: PostgresType
}

/// An iterator over the resulting rows of a query.
pub struct PostgresRows<'stmt> {
    stmt: &'stmt PostgresStatement<'stmt>,
    name: ~str,
    data: RingBuf<Vec<Option<Vec<u8>>>>,
    row_limit: uint,
    more_rows: bool,
    finished: bool,
}

#[unsafe_destructor]
impl<'stmt> Drop for PostgresRows<'stmt> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish_inner();
        }
    }
}

impl<'stmt> PostgresRows<'stmt> {
    fn finish_inner(&mut self) -> PostgresResult<()> {
        check_desync!(self.stmt.conn);
        try_pg!(self.stmt.conn.write_messages([
            Close {
                variant: 'P' as u8,
                name: self.name.as_slice()
            },
            Sync]));
        loop {
            match try_pg!(self.stmt.conn.read_message()) {
                ReadyForQuery { .. } => break,
                ErrorResponse { fields } => {
                    try!(self.stmt.conn.wait_for_ready());
                    return Err(PgDbError(PostgresDbError::new(fields)));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn read_rows(&mut self) -> PostgresResult<()> {
        loop {
            match try_pg!(self.stmt.conn.read_message()) {
                EmptyQueryResponse |
                CommandComplete { .. } => {
                    self.more_rows = false;
                    break;
                },
                PortalSuspended => {
                    self.more_rows = true;
                    break;
                },
                DataRow { row } => self.data.push_back(row),
                _ => unreachable!()
            }
        }
        self.stmt.conn.wait_for_ready()
    }

    fn execute(&mut self) -> PostgresResult<()> {
        try_pg!(self.stmt.conn.write_messages([
            Execute {
                portal: self.name,
                max_rows: self.row_limit as i32
            },
            Sync]));
        self.read_rows()
    }

    /// Consumes the `PostgresRows`, cleaning up associated state.
    ///
    /// Functionally identical to the `Drop` implementation on `PostgresRows`
    /// except that it returns any error to the caller.
    #[inline]
    pub fn finish(mut self) -> PostgresResult<()> {
        self.finished = true;
        self.finish_inner()
    }

    fn try_next(&mut self) -> Option<PostgresResult<PostgresRow<'stmt>>> {
        if self.data.is_empty() && self.more_rows {
            match self.execute() {
                Ok(()) => {}
                Err(err) => return Some(Err(err))
            }
        }

        self.data.pop_front().map(|row| {
            Ok(PostgresRow {
                stmt: self.stmt,
                data: row
            })
        })
    }
}

impl<'stmt> Iterator<PostgresRow<'stmt>> for PostgresRows<'stmt> {
    #[inline]
    fn next(&mut self) -> Option<PostgresRow<'stmt>> {
        // we'll never hit the network on a non-lazy result
        self.try_next().map(|r| r.unwrap())
    }

    #[inline]
    fn size_hint(&self) -> (uint, Option<uint>) {
        let lower = self.data.len();
        let upper = if self.more_rows {
            None
         } else {
            Some(lower)
         };
         (lower, upper)
    }
}

/// A single result row of a query.
pub struct PostgresRow<'stmt> {
    stmt: &'stmt PostgresStatement<'stmt>,
    data: Vec<Option<Vec<u8>>>
}

impl<'stmt> PostgresRow<'stmt> {
    /// Retrieves the contents of a field of the row.
    ///
    /// A field can be accessed by the name or index of its column, though
    /// access by index is more efficient. Rows are 1-indexed.
    ///
    /// Returns an `Error` value if the index does not reference a column or
    /// the return type is not compatible with the Postgres type.
    pub fn get<I: RowIndex, T: FromSql>(&self, idx: I) -> PostgresResult<T> {
        let idx = match idx.idx(self.stmt) {
            Some(idx) => idx,
            None => return Err(PgInvalidColumn)
        };
        FromSql::from_sql(&self.stmt.result_desc.get(idx).ty,
                          self.data.get(idx))
    }
}

impl<'stmt> Container for PostgresRow<'stmt> {
    #[inline]
    fn len(&self) -> uint {
        self.data.len()
    }
}

impl<'stmt, I: RowIndex+Clone+fmt::Show, T: FromSql> Index<I, T>
        for PostgresRow<'stmt> {
    /// Retreives the contents of a field of the row.
    ///
    /// A field can be accessed by the name or index of its column, though
    /// access by index is more efficient. Rows are 1-indexed.
    ///
    /// # Failure
    ///
    /// Fails if the index does not reference a column or the return type is
    /// not compatible with the Postgres type.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use postgres::{PostgresConnection, NoSsl};
    /// # let conn = PostgresConnection::connect("", &NoSsl).unwrap();
    /// # let stmt = conn.prepare("").unwrap();
    /// # let mut result = stmt.query([]).unwrap();
    /// # let row = result.next().unwrap();
    /// let foo: i32 = row[1];
    /// let bar: ~str = row["bar"];
    /// ```
    fn index(&self, idx: &I) -> T {
        match self.get(idx.clone()) {
            Ok(ok) => ok,
            Err(err) => fail!("error retrieving column {}: {}", idx, err)
        }
    }
}

/// A trait implemented by types that can index into columns of a row.
pub trait RowIndex {
    /// Returns the index of the appropriate column, or `None` if no such
    /// column exists.
    fn idx(&self, stmt: &PostgresStatement) -> Option<uint>;
}

impl RowIndex for uint {
    #[inline]
    fn idx(&self, stmt: &PostgresStatement) -> Option<uint> {
        if *self == 0 || *self > stmt.result_desc.len() {
            None
        } else {
            Some(*self - 1)
        }
    }
}

// This is a convenience as the 1 in get[1] resolves to int :(
impl RowIndex for int {
    #[inline]
    fn idx(&self, stmt: &PostgresStatement) -> Option<uint> {
        if *self < 0 {
            return None;
        }

        (*self as uint).idx(stmt)
    }
}

impl<'a> RowIndex for &'a str {
    fn idx(&self, stmt: &PostgresStatement) -> Option<uint> {
        for (i, desc) in stmt.result_descriptions().iter().enumerate() {
            if desc.name.as_slice() == *self {
                return Some(i);
            }
        }
        None
    }
}

/// A lazily-loaded iterator over the resulting rows of a query
pub struct PostgresLazyRows<'trans, 'stmt> {
    result: PostgresRows<'stmt>,
    trans: &'trans PostgresTransaction<'trans>,
}

impl<'trans, 'stmt> PostgresLazyRows<'trans, 'stmt> {
    /// Like `PostgresRows::finish`.
    #[inline]
    pub fn finish(self) -> PostgresResult<()> {
        self.result.finish()
    }
}

impl<'trans, 'stmt> Iterator<PostgresResult<PostgresRow<'stmt>>>
        for PostgresLazyRows<'trans, 'stmt> {
    #[inline]
    fn next(&mut self) -> Option<PostgresResult<PostgresRow<'stmt>>> {
        self.result.try_next()
    }

    #[inline]
    fn size_hint(&self) -> (uint, Option<uint>) {
        self.result.size_hint()
    }
}

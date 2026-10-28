//! A throwaway database per test on the server `SEKISHO_TEST_POSTGRES_URL`
//! names (a plain `postgres://` URL of a user that may create databases).

use std::path::PathBuf;

/// A database of its own on the test server, dropped with this value.
pub struct TestDb {
    admin: ::postgres::Config,
    pub name: String,
    pub url_file: PathBuf,
    _dir: tempfile::TempDir,
}

/// Run `work` on its own thread: the blocking client must not run on an
/// async worker.
fn blocking<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| scope.spawn(work).join().unwrap())
}

fn run(config: &::postgres::Config, sql: &str) {
    blocking(|| {
        let mut client = config.connect(::postgres::NoTls).unwrap();
        client.batch_execute(sql).unwrap();
    });
}

impl TestDb {
    /// `None`, after saying so, when no test server is configured.
    pub fn create() -> Option<Self> {
        let Ok(url) = std::env::var("SEKISHO_TEST_POSTGRES_URL") else {
            eprintln!("skipped: SEKISHO_TEST_POSTGRES_URL is not set");
            return None;
        };
        let admin: ::postgres::Config = url.parse().unwrap();
        let name = format!(
            "sekisho_test_{}_{}",
            std::process::id(),
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        run(&admin, &format!("CREATE DATABASE {name}"));
        let mut database = url::Url::parse(&url).unwrap();
        database.set_path(&format!("/{name}"));
        let dir = tempfile::tempdir().unwrap();
        let url_file = dir.path().join("postgres-url");
        std::fs::write(&url_file, database.as_str()).unwrap();
        Some(Self {
            admin,
            name,
            url_file,
            _dir: dir,
        })
    }

    /// Run `sql` in this database.
    pub fn execute(&self, sql: &str) {
        let mut config = self.admin.clone();
        config.dbname(&self.name);
        run(&config, sql);
    }

    /// Refuse new connections to this database and end the open ones,
    /// as a store outage does; `false` lets connections in again.
    pub fn outage(&self, down: bool) {
        let name = &self.name;
        run(
            &self.admin,
            &if down {
                format!(
                    "ALTER DATABASE {name} ALLOW_CONNECTIONS false;
                     SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{name}';"
                )
            } else {
                format!("ALTER DATABASE {name} ALLOW_CONNECTIONS true")
            },
        );
    }
}

/// A transaction another session keeps open until [`Held::release`].
pub struct Held {
    release: std::sync::mpsc::Sender<()>,
    session: std::thread::JoinHandle<()>,
}

impl Held {
    /// Commit the transaction.
    pub fn release(self) {
        self.release.send(()).unwrap();
        self.session.join().unwrap();
    }
}

impl TestDb {
    /// Run `sql` in a transaction of this database and keep it open,
    /// with its locks, until released.
    pub fn hold(&self, sql: &str) -> Held {
        let mut config = self.admin.clone();
        config.dbname(&self.name);
        let sql = format!("BEGIN; {sql}");
        let (ready, started) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let session = std::thread::spawn(move || {
            let mut client = config.connect(::postgres::NoTls).unwrap();
            client.batch_execute(&sql).unwrap();
            ready.send(()).unwrap();
            let _ = released.recv();
            client.batch_execute("COMMIT").unwrap();
        });
        started.recv().unwrap();
        Held { release, session }
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        run(
            &self.admin,
            &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name),
        );
    }
}

//! A small pool of our own (ARCHITECTURE, Backends): at most `store_connection_count_max`
//! connections, each waited for at most `store_connection_acquire` (PRACTICES, Explicit
//! limits). Every connection enforces foreign keys, which Turso sets per connection.

use std::sync::{Mutex, PoisonError};

use cairn_store::StoreError;
use cairn_store::limits::{STORE_CONNECTION_ACQUIRE, STORE_CONNECTION_COUNT_MAX};
use tokio::sync::{Semaphore, SemaphorePermit};
use turso::{Connection, Database};

use crate::sql::execute;

/// The pool.
pub(crate) struct Pool {
    database: Database,
    idle: Mutex<Vec<Connection>>,
    permits: Semaphore,
}

/// A connection on loan. It goes back to the pool only when [`Lease::release`] says it is
/// outside any transaction; otherwise it is dropped, which rolls back what it held.
pub(crate) struct Lease<'pool> {
    pool: &'pool Pool,
    connection: Option<Connection>,
    _permit: SemaphorePermit<'pool>,
}

impl Pool {
    pub fn new(database: Database) -> Self {
        let permits = usize::try_from(STORE_CONNECTION_COUNT_MAX).unwrap_or(usize::MAX);
        Self {
            database,
            idle: Mutex::new(Vec::new()),
            permits: Semaphore::new(permits),
        }
    }

    /// A connection, waiting at most the acquire limit for one to come free.
    pub async fn acquire(&self) -> Result<Lease<'_>, StoreError> {
        crate::wait_for_store().await;
        let permit = tokio::time::timeout(STORE_CONNECTION_ACQUIRE, self.permits.acquire())
            .await
            .map_err(|_| StoreError::AcquireTimeout)?
            .map_err(|error| StoreError::Backend(format!("the pool is closed: {error}")))?;
        let idle = self
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        let connection = match idle {
            Some(connection) => connection,
            None => self.connect().await?,
        };
        Ok(Lease {
            pool: self,
            connection: Some(connection),
            _permit: permit,
        })
    }

    /// A new connection with foreign keys on.
    pub async fn connect(&self) -> Result<Connection, StoreError> {
        let connection = self
            .database
            .connect()
            .map_err(|error| StoreError::Backend(format!("connect: {error}")))?;
        execute(&connection, "PRAGMA foreign_keys = ON", Vec::new()).await?;
        Ok(connection)
    }
}

impl Lease<'_> {
    pub fn connection(&self) -> &Connection {
        // Present until release or drop.
        self.connection
            .as_ref()
            .unwrap_or_else(|| unreachable!("a lease holds its connection until it ends"))
    }

    /// Returns the connection to the pool: it is outside any transaction.
    pub fn release(mut self) {
        if let Some(connection) = self.connection.take() {
            self.pool
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(connection);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(future)
    }

    async fn pool() -> Pool {
        Pool::new(turso::Builder::new_local(":memory:").build().await.unwrap())
    }

    #[test]
    fn every_connection_enforces_foreign_keys() {
        run(async {
            let pool = pool().await;
            let lease = pool.acquire().await.unwrap();
            let row = crate::sql::first(lease.connection(), "PRAGMA foreign_keys", Vec::new())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.int(0).unwrap(), 1);
        });
    }

    #[test]
    fn a_lease_past_the_connection_limit_waits_out_the_acquire_limit() {
        run(async {
            let pool = pool().await;
            let mut leases = Vec::new();
            for _ in 0..STORE_CONNECTION_COUNT_MAX {
                leases.push(pool.acquire().await.unwrap());
            }
            let started = tokio::time::Instant::now();
            assert!(matches!(
                pool.acquire().await,
                Err(StoreError::AcquireTimeout)
            ));
            assert_eq!(started.elapsed(), STORE_CONNECTION_ACQUIRE);
            leases.pop().unwrap().release();
            assert!(pool.acquire().await.is_ok());
        });
    }
}

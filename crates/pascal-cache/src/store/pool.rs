use super::limits::StoreLimits;
use super::outcome::Bypass;
use super::sqlite::{classify, open_connection};
use rusqlite::Connection;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The cumulative wait allowance of one cache operation (spec §10): time
/// spent waiting for a pooled connection and for SQLite locks. Every wait
/// of the operation is bounded by what remains, so waits add up.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadline(Instant);

impl Deadline {
    pub(crate) fn after(wait: Duration) -> Self {
        Self(Instant::now() + wait)
    }

    pub(crate) fn remaining(&self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Reader,
    Writer,
}

struct PoolState {
    idle_readers: Vec<Connection>,
    open_readers: usize,
    writer: Option<Connection>,
    writer_open: bool,
}

/// At most `limits.readers` read connections and one write connection,
/// opened lazily and reused.
pub(crate) struct Pool {
    database: PathBuf,
    limits: StoreLimits,
    state: Mutex<PoolState>,
    returned: Condvar,
}

pub(crate) struct Lease<'a> {
    pool: &'a Pool,
    role: Role,
    connection: Option<Connection>,
}

impl Pool {
    pub(crate) fn new(database: PathBuf, limits: StoreLimits) -> Self {
        Self {
            database,
            limits,
            state: Mutex::new(PoolState {
                idle_readers: Vec::new(),
                open_readers: 0,
                writer: None,
                writer_open: false,
            }),
            returned: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn open_connections(&self) -> usize {
        let state = self.lock();
        state.open_readers + usize::from(state.writer_open)
    }

    pub(crate) fn acquire(&self, role: Role, deadline: Deadline) -> Result<Lease<'_>, Bypass> {
        let mut state = self.lock();
        loop {
            match role {
                Role::Writer => {
                    if let Some(connection) = state.writer.take() {
                        drop(state);
                        return self.lease(role, connection, deadline);
                    }
                    if !state.writer_open {
                        state.writer_open = true;
                        drop(state);
                        return self.open(role, deadline);
                    }
                }
                Role::Reader => {
                    if let Some(connection) = state.idle_readers.pop() {
                        drop(state);
                        return self.lease(role, connection, deadline);
                    }
                    if state.open_readers < self.limits.readers {
                        state.open_readers += 1;
                        drop(state);
                        return self.open(role, deadline);
                    }
                }
            }
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return Err(Bypass::Busy);
            }
            state = self
                .returned
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn open(&self, role: Role, deadline: Deadline) -> Result<Lease<'_>, Bypass> {
        match open_connection(
            &self.database,
            self.limits.connection_page_cache_bytes(),
            deadline.remaining(),
        ) {
            Ok(connection) => self.lease(role, connection, deadline),
            Err(bypass) => {
                let mut state = self.lock();
                match role {
                    Role::Writer => state.writer_open = false,
                    Role::Reader => state.open_readers -= 1,
                }
                self.returned.notify_all();
                Err(bypass)
            }
        }
    }

    fn lease(
        &self,
        role: Role,
        connection: Connection,
        deadline: Deadline,
    ) -> Result<Lease<'_>, Bypass> {
        let lease = Lease {
            pool: self,
            role,
            connection: Some(connection),
        };
        // SQLite lock waits count against the same allowance.
        lease
            .busy_timeout(deadline.remaining())
            .map_err(|error| classify(&error))?;
        Ok(lease)
    }
}

impl Deref for Lease<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.connection.as_ref().expect("leased connection")
    }
}

impl DerefMut for Lease<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.connection.as_mut().expect("leased connection")
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            let mut state = self.pool.lock();
            match self.role {
                Role::Writer => state.writer = Some(connection),
                Role::Reader => state.idle_readers.push(connection),
            }
            self.pool.returned.notify_all();
        }
    }
}

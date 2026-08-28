//! A hybrid logical clock: wall time when it moves forward, a counter when it
//! does not, so writes from one site are totally ordered even across a clock
//! regression, and a received timestamp can never be overtaken by a local one.

use rusqlite::Connection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hlc {
    pub last_ms: i64,
    pub last_ctr: u32,
}

impl Hlc {
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        conn.query_row("SELECT last_ms, last_ctr FROM hlc WHERE id = 1", [], |r| {
            Ok(Self {
                last_ms: r.get(0)?,
                last_ctr: r.get::<_, i64>(1)? as u32,
            })
        })
    }

    pub fn store(&self, conn: &Connection) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE hlc SET last_ms = ?1, last_ctr = ?2 WHERE id = 1",
            rusqlite::params![self.last_ms, self.last_ctr as i64],
        )?;
        Ok(())
    }

    /// A stamp for a local write.
    pub fn tick(&mut self, now_ms: i64) -> Option<(i64, u32)> {
        let next = if now_ms > self.last_ms {
            (now_ms, 0)
        } else {
            bump(self.last_ms, self.last_ctr)?
        };
        (self.last_ms, self.last_ctr) = next;
        Some(next)
    }

    /// Fold in a stamp seen from another site, so the next local write sorts
    /// after it.
    pub fn observe(&mut self, now_ms: i64, remote: (i64, u32)) -> Option<(i64, u32)> {
        let (rms, rctr) = remote;
        let next = if now_ms > self.last_ms && now_ms > rms {
            (now_ms, 0)
        } else if rms > self.last_ms {
            bump(rms, rctr)?
        } else if rms == self.last_ms {
            bump(self.last_ms, self.last_ctr.max(rctr))?
        } else {
            bump(self.last_ms, self.last_ctr)?
        };
        (self.last_ms, self.last_ctr) = next;
        Some(next)
    }
}

/// Advance one logical step. Exhausting the counter borrows one millisecond;
/// the only impossible successor is the largest representable stamp.
fn bump(ms: i64, ctr: u32) -> Option<(i64, u32)> {
    match ctr.checked_add(1) {
        Some(next) if !(ms == i64::MAX && next == u32::MAX) => Some((ms, next)),
        Some(_) => None,
        None => ms.checked_add(1).map(|next_ms| (next_ms, 0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_are_monotonic_through_a_clock_regression() {
        let mut h = Hlc {
            last_ms: 0,
            last_ctr: 0,
        };
        let a = h.tick(1000).unwrap();
        let b = h.tick(900).unwrap(); // the wall clock went backwards
        let c = h.tick(900).unwrap();
        let d = h.tick(1001).unwrap();
        assert!(a < b && b < c && c < d, "{a:?} {b:?} {c:?} {d:?}");
        assert_eq!(b, (1000, 1));
        assert_eq!(d, (1001, 0));
    }

    #[test]
    fn an_observed_stamp_is_never_overtaken() {
        let mut h = Hlc {
            last_ms: 500,
            last_ctr: 0,
        };
        let seen = h.observe(400, (2000, 3)).unwrap();
        assert_eq!(seen, (2000, 4));
        assert!(h.tick(400).unwrap() > (2000, 3));
        let mut h = Hlc {
            last_ms: 2000,
            last_ctr: 9,
        };
        assert_eq!(h.observe(1000, (2000, 3)).unwrap(), (2000, 10));
        assert_eq!(h.observe(3000, (2000, 3)).unwrap(), (3000, 0));
    }

    #[test]
    fn counter_overflow_advances_the_logical_millisecond_or_refuses_exhaustion() {
        let mut h = Hlc {
            last_ms: 1000,
            last_ctr: u32::MAX,
        };
        assert_eq!(h.tick(0), Some((1001, 0)));

        let mut exhausted = Hlc {
            last_ms: i64::MAX,
            last_ctr: u32::MAX - 1,
        };
        assert_eq!(exhausted.observe(0, (i64::MAX, u32::MAX)), None);
        assert_eq!(exhausted.last_ctr, u32::MAX - 1);
    }

    #[test]
    fn round_trips_through_the_table() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::install(&conn).unwrap();
        let mut h = Hlc::load(&conn).unwrap();
        h.tick(42).unwrap();
        h.store(&conn).unwrap();
        assert_eq!(Hlc::load(&conn).unwrap(), h);
    }
}

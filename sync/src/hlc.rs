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
    pub fn tick(&mut self, now_ms: i64) -> (i64, u32) {
        if now_ms > self.last_ms {
            self.last_ms = now_ms;
            self.last_ctr = 0;
        } else {
            self.last_ctr += 1;
        }
        (self.last_ms, self.last_ctr)
    }

    /// Fold in a stamp seen from another site, so the next local write sorts
    /// after it.
    pub fn observe(&mut self, now_ms: i64, remote: (i64, u32)) -> (i64, u32) {
        let (rms, rctr) = remote;
        if now_ms > self.last_ms && now_ms > rms {
            self.last_ms = now_ms;
            self.last_ctr = 0;
        } else if rms > self.last_ms {
            self.last_ms = rms;
            self.last_ctr = rctr + 1;
        } else if rms == self.last_ms {
            self.last_ctr = self.last_ctr.max(rctr) + 1;
        } else {
            self.last_ctr += 1;
        }
        (self.last_ms, self.last_ctr)
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
        let a = h.tick(1000);
        let b = h.tick(900); // the wall clock went backwards
        let c = h.tick(900);
        let d = h.tick(1001);
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
        let seen = h.observe(400, (2000, 3));
        assert_eq!(seen, (2000, 4));
        assert!(h.tick(400) > (2000, 3));
        let mut h = Hlc {
            last_ms: 2000,
            last_ctr: 9,
        };
        assert_eq!(h.observe(1000, (2000, 3)), (2000, 10));
        assert_eq!(h.observe(3000, (2000, 3)), (3000, 0));
    }

    #[test]
    fn round_trips_through_the_table() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::install(&conn).unwrap();
        let mut h = Hlc::load(&conn).unwrap();
        h.tick(42);
        h.store(&conn).unwrap();
        assert_eq!(Hlc::load(&conn).unwrap(), h);
    }
}

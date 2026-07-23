//! Gamification helpers shared by status reporting and the learner.
//! Companion chats use real `type='nomi'` threads driven by the full agent
//! engine; see `companion.rs`.

/// Level curve: Lv = floor(sqrt(xp/100)) + 1.
pub fn level_for_xp(xp: i64) -> i64 {
    ((xp.max(0) as f64 / 100.0).sqrt() as i64) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_curve() {
        assert_eq!(level_for_xp(0), 1);
        assert_eq!(level_for_xp(99), 1);
        assert_eq!(level_for_xp(100), 2);
        assert_eq!(level_for_xp(400), 3);
        assert_eq!(level_for_xp(1600), 5);
    }
}

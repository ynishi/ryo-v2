//! cfg-test-lab: code that only `cfg(test)` and an integration test exercise.

/// The value the tests check.
pub fn helper() -> u32 {
    scale(21)
}

fn scale(n: u32) -> u32 {
    n * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_doubles() {
        assert_eq!(scale(21), 42);
    }

    #[test]
    fn helper_is_42() {
        assert_eq!(helper(), 42);
        let _ = missing_fn();
    }
}

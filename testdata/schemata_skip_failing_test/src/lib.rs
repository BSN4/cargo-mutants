//! A test that fails while the schemata record coverage, standing in for a test that counts
//! allocations, which the recording makes.

pub fn answer() -> u32 {
    42
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn answer_is_42_when_not_traced() {
        assert!(std::env::var_os("CARGO_MUTANTS_SCHEMATA_TRACE").is_none());
        assert_eq!(answer(), 42);
    }

    #[test]
    fn something_else() {}
}

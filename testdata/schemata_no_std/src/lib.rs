//! A `no_std` crate with code that can't be switched at runtime in a schemata build,
//! beside code that can.

#![no_std]

/// A const fn: its mutants can't read a runtime switch.
pub const fn double(x: u32) -> u32 {
    x * 2
}

/// Replacements for `impl Iterator` are different types from the original.
pub fn evens(limit: u32) -> impl Iterator<Item = u32> {
    (0..limit).filter(|x| x % 2 == 0)
}

/// The `&&` joining a `let` condition can't be wrapped in an expression.
pub fn first_even(xs: &[u32]) -> Option<u32> {
    if let Some(&x) = xs.first()
        && x % 2 == 0
    {
        Some(x)
    } else {
        None
    }
}

pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn double_doubles() {
        assert_eq!(double(3), 6);
    }

    #[test]
    fn evens_are_even() {
        let mut evens = evens(5);
        assert_eq!(evens.next(), Some(0));
        assert_eq!(evens.next(), Some(2));
        assert_eq!(evens.next(), Some(4));
        assert_eq!(evens.next(), None);
    }

    #[test]
    fn first_even_finds_only_an_even_first() {
        assert_eq!(first_even(&[2, 3]), Some(2));
        assert_eq!(first_even(&[3, 2]), None);
        assert_eq!(first_even(&[]), None);
    }

    #[test]
    fn add_adds() {
        assert_eq!(add(2, 3), 5);
    }
}

//! Short random session ids in the style of nanoid.

/// Lowercase letters and digits with look-alikes (0 o, 1 l i, 2 z, 5 s, u v)
/// and all vowels removed, so an id is easy to read aloud and cannot spell a
/// word. 20 symbols at 6 characters give 64 million ids; callers retry on the
/// rare collision.
const ALPHABET: &[u8; 20] = b"6789bcdfghjkmnpqrtwx";
pub const LENGTH: usize = 6;
/// Ids are also directory names and URL segments. Rows from before random ids
/// are plain numbers, so digits outside the alphabet stay valid.
const MAX_LENGTH: usize = 12;

pub fn generate() -> String {
    let mut id = String::with_capacity(LENGTH);
    while id.len() < LENGTH {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("the OS must provide randomness");
        // Masking to 0..32 and rejecting values past the alphabet keeps every
        // symbol equally likely.
        let symbols = bytes
            .iter()
            .map(|byte| (byte & 31) as usize)
            .filter(|index| *index < ALPHABET.len())
            .map(|index| ALPHABET[index] as char);
        id.extend(symbols.take(LENGTH - id.len()));
    }
    id
}

pub fn is_valid(id: &str) -> bool {
    (1..=MAX_LENGTH).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_use_the_alphabet_and_validate() {
        for _ in 0..1000 {
            let id = generate();
            assert_eq!(id.len(), LENGTH);
            assert!(id.bytes().all(|b| ALPHABET.contains(&b)), "{id}");
            assert!(is_valid(&id));
        }
    }

    #[test]
    fn malformed_ids_are_rejected() {
        for id in ["", "..", "a/b", "ABC", "a b", "abcdefghijklm"] {
            assert!(!is_valid(id), "{id}");
        }
        assert!(
            is_valid("12"),
            "numeric ids from before random ids stay valid"
        );
    }
}

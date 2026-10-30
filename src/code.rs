//! Pairing codes like `48213-otter-violin`: short enough to read out or type. The number is where the two
//! devices meet: both turn it into the same temporary address, which the other device dials. The words
//! are a password the devices check with SPAKE2, a password-authenticated key exchange: each proves it
//! knows the code without sending it, so someone listening learns nothing, and someone guessing gets one
//! try in 65,536 before the code is used up.

use anyhow::{Context, Result, anyhow, bail, ensure};
use iroh::{SecretKey, endpoint::Connection};
use spake2::{Ed25519Group, Identity, Password, Spake2};

/// 256 everyday words, so two of them make 16 bits; none of them sounds like another common word.
const WORDS: [&str; 256] = [
    "almond", "anchor", "apple", "apricot", "arrow", "atlas", "bacon", "badge", "badger", "bagel", "banana", "banjo",
    "banner", "barrel", "basil", "basket", "beacon", "beaver", "bicycle", "biscuit", "bison", "blanket", "bottle", "bucket",
    "burger", "butter", "button", "cabbage", "cabin", "camel", "camera", "candle", "canoe", "canyon", "carpet", "carrot",
    "cashew", "castle", "celery", "cello", "cheese", "cherry", "chimney", "cliff", "clock", "cloud", "coast", "cobra",
    "cocoa", "coconut", "comet", "compass", "cookie", "crab", "cracker", "cradle", "crane", "crater", "crayon", "crow",
    "crown", "cumin", "curry", "cushion", "desert", "dingo", "dolphin", "donkey", "drum", "duck", "dune", "eagle",
    "easel", "engine", "falcon", "ferret", "fiddle", "finch", "flute", "forest", "fox", "frog", "frost", "funnel",
    "gadget", "galaxy", "garden", "garlic", "gecko", "geyser", "ginger", "glacier", "goat", "goose", "gopher", "grape",
    "gravy", "guitar", "hammer", "hawk", "helmet", "heron", "hippo", "honey", "hyena", "iguana", "island", "jaguar",
    "jungle", "kettle", "kitten", "kiwi", "koala", "ladder", "lagoon", "lantern", "lemon", "lemur", "lentil", "lettuce",
    "lion", "lizard", "llama", "lobster", "locket", "magnet", "magpie", "mango", "marble", "meadow", "melon", "mirror",
    "mitten", "mole", "monkey", "mouse", "muffin", "mule", "mustard", "needle", "newt", "noodle", "nutmeg", "oatmeal",
    "ocean", "octopus", "olive", "onion", "orange", "orbit", "otter", "owl", "oyster", "paddle", "panda", "paprika",
    "parcel", "parrot", "pasta", "peanut", "pebble", "pelican", "pencil", "penguin", "pepper", "piano", "pickle", "pigeon",
    "pillow", "pizza", "planet", "pocket", "pony", "popcorn", "potato", "prairie", "pretzel", "pudding", "puddle", "puffin",
    "pumpkin", "puppy", "puzzle", "quilt", "rabbit", "radio", "radish", "rainbow", "raisin", "raven", "rhino", "ribbon",
    "river", "robin", "rocket", "saddle", "salad", "salmon", "salsa", "sandal", "sausage", "scarf", "seal", "sesame",
    "shark", "sheep", "shore", "shovel", "shrimp", "sled", "sloth", "snail", "snake", "spider", "spinach", "spoon",
    "squid", "statue", "sugar", "summit", "sunset", "sushi", "swamp", "swan", "taco", "teapot", "thunder", "ticket",
    "tiger", "toffee", "tomato", "trout", "trumpet", "tuna", "tundra", "tunnel", "turkey", "turnip", "turtle", "valley",
    "vanilla", "velvet", "violin", "volcano", "waffle", "wagon", "wallet", "walnut", "walrus", "whistle", "window", "wolf",
    "wombat", "yogurt", "zebra", "zipper",
];

/// A pairing code: a 5-digit meeting number and two words.
#[derive(Clone, Debug, PartialEq)]
pub struct Code {
    number: u32,
    words: [u8; 2],
}

impl Code {
    pub fn generate() -> Code {
        let random = SecretKey::generate().to_bytes(); // from the OS random generator, like toss's other secrets
        let number = u32::from_le_bytes([random[0], random[1], random[2], random[3]]) % 100_000;
        Code { number, words: [random[4], random[5]] }
    }

    /// Reads a code as people type it: any case, with dashes or spaces between the parts.
    pub fn parse(typed: &str) -> Result<Code> {
        let typed = typed.trim().to_lowercase();
        let parts: Vec<&str> = typed.split(|c: char| c == '-' || c.is_whitespace()).filter(|p| !p.is_empty()).collect();
        let [number, first, second] = parts[..] else { bail!("a pairing code looks like 48213-otter-violin") };
        ensure!(number.len() == 5 && number.bytes().all(|b| b.is_ascii_digit()), "a pairing code starts with 5 digits, like 48213");
        let word = |w: &str| {
            let at = WORDS.iter().position(|known| *known == w).ok_or_else(|| anyhow!("\"{w}\" isn't one of toss's words: check the code"))?;
            anyhow::Ok(at as u8)
        };
        Ok(Code { number: number.parse()?, words: [word(first)?, word(second)?] })
    }

    /// The key of the temporary address both devices work out from the number: the inviting device listens
    /// there. Anyone with the number can work it out too, so it only says where to meet; SPAKE2 proves who.
    pub fn meeting(&self) -> SecretKey {
        SecretKey::from_bytes(&blake3::derive_key("toss 2026-10 pairing code meeting", format!("{:05}", self.number).as_bytes()))
    }

    /// Starts SPAKE2 for the dialing side (the one that typed the code) or the inviting one: its state, and
    /// the message to send the other side.
    pub fn pake(&self, dialing: bool) -> (Spake2<Ed25519Group>, Vec<u8>) {
        let (password, dialer, inviter) = (Password::new(self.to_string()), Identity::new(b"toss dialer"), Identity::new(b"toss inviter"));
        if dialing { Spake2::start_a(&password, &dialer, &inviter) } else { Spake2::start_b(&password, &dialer, &inviter) }
    }
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:05}-{}-{}", self.number, WORDS[usize::from(self.words[0])], WORDS[usize::from(self.words[1])])
    }
}

/// Proof that the device `from`, in `role` on `conn`, finished SPAKE2 with the same code: a MAC under the
/// shared `key` over this connection's own TLS keys, so a device in the middle can't pass it on to another
/// connection, and over the device's real id.
pub fn proof(key: &[u8], conn: &Connection, role: &[u8], from: &str) -> Result<Vec<u8>> {
    let key: &[u8; 32] = key.try_into().context("SPAKE2 gave a key of the wrong size")?;
    let mut mac = blake3::Hasher::new_keyed(key);
    mac.update(&crate::binding(conn, role)?);
    mac.update(from.as_bytes());
    Ok(mac.finalize().as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_read_back_however_they_are_typed() {
        for _ in 0..20 {
            let code = Code::generate();
            assert_eq!(Code::parse(&code.to_string()).unwrap(), code);
        }
        let code = Code { number: 4821, words: [0, 255] };
        assert_eq!(code.to_string(), "04821-almond-zipper");
        for typed in ["04821-almond-zipper", " 04821 Almond ZIPPER ", "04821 - almond - zipper"] {
            assert_eq!(Code::parse(typed).unwrap(), code, "{typed}");
        }
        for wrong in ["4821-almond-zipper", "04821-almond", "04821-almond-zippers", "toss:abc/def", ""] {
            assert!(Code::parse(wrong).is_err(), "{wrong}");
        }
        assert_eq!(code.meeting().public(), Code::parse("04821-otter-violin").unwrap().meeting().public(), "the number decides where to meet");
        assert_ne!(code.meeting().public(), Code { number: 4822, words: [0, 255] }.meeting().public());
    }

    #[test]
    fn the_words_are_distinct_and_plain() {
        let mut sorted = WORDS.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 256, "no word twice");
        assert!(WORDS.iter().all(|w| (3..=7).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_lowercase())));
    }
}

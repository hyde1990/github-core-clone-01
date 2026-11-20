//! The kit's seeded scenario (wire.ts): FNV-1a 64, a splitmix64 finalizer,
//! xorshift64*, and the user/org generators. The forge needs the token
//! owner's login at startup, and the only source of it is this deterministic
//! generation from the public seed.

pub const SCENARIO_SEED: u64 = 0x6a09e667f3bcc909;
pub const SCENARIO_EPOCH_SEC: u64 = 1_735_689_600;

const FIRST_NAMES: &[&str] = &[
    "ada", "bram", "cora", "dmitri", "elena", "felix", "greta", "hugo", "iris", "jonas", "kira",
    "lars", "mira", "nico", "olga", "pavel", "quinn", "rosa", "sven", "tess", "ulla", "vito",
    "wren", "yara", "zane",
];
const LAST_NAMES: &[&str] = &[
    "anders", "bauer", "costa", "dahl", "eriksen", "fischer", "garcia", "holm", "iversen",
    "jansen", "keller", "lind", "moreau", "novak", "olsen", "peters", "quist", "rossi", "sato",
    "turi", "ulrich", "vega", "weber", "xu", "yilmaz", "zimmer",
];
const REPO_ADJECTIVES: &[&str] = &[
    "amber", "brisk", "calm", "crisp", "eager", "faint", "gentle", "happy", "idle", "jolly",
    "keen", "lively", "mellow", "nimble", "open", "plain", "quick", "quiet", "rapid", "soft",
    "steady", "tidy", "upbeat", "vivid", "warm", "witty", "young", "zesty",
];
const REPO_NOUNS: &[&str] = &[
    "acorn", "badger", "canyon", "cedar", "comet", "coral", "delta", "ember", "falcon", "glade",
    "harbor", "indigo", "juniper", "kettle", "lantern", "meadow", "nebula", "orchid", "pebble",
    "quartz", "raven", "saffron", "thistle", "umber", "violet", "willow", "yarrow", "zephyr",
];

fn fnv1a64(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// splitmix64 finalizer (wire.ts `mix64`).
fn mix64(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9e3779b97f4a7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

fn seed_for(label: &str) -> u64 {
    mix64(SCENARIO_SEED ^ fnv1a64(label))
}

/// xorshift64* (wire.ts Rng).
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn from_label(label: &str) -> Self {
        Rng::new(seed_for(label))
    }

    pub fn new(seed: u64) -> Self {
        Rng {
            state: if seed == 0 { SCENARIO_SEED } else { seed },
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545f4914f6cdd1d)
    }

    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// An integer in [0, max), unbiased by rejection (wire.ts nextInt).
    pub fn next_int(&mut self, max: u32) -> u32 {
        if max == 1 {
            return 0;
        }
        let range: u64 = 4_294_967_296;
        let limit = range - (range % max as u64);
        loop {
            let value = self.next_u32() as u64;
            if value < limit {
                return (value % max as u64) as u32;
            }
        }
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.next_int(items.len() as u32) as usize]
    }
}

#[derive(Clone, Debug)]
pub struct User {
    #[allow(dead_code)]
    pub id: u32,
    pub login: String,
    pub name: String,
    pub email: String,
}

pub fn generate_users(rng: &mut Rng, count: usize) -> Vec<User> {
    let mut taken = std::collections::HashSet::new();
    let mut users = Vec::with_capacity(count);
    for i in 0..count {
        let first = rng.pick(FIRST_NAMES);
        let last = rng.pick(LAST_NAMES);
        let base = format!("{first}{last}");
        let mut login = base.clone();
        let mut suffix = 2;
        while taken.contains(&login) {
            login = format!("{base}-{suffix}");
            suffix += 1;
        }
        taken.insert(login.clone());
        users.push(User {
            id: 1000 + i as u32,
            login: login.clone(),
            name: format!("{first} {last}"),
            email: format!("{login}@example.invalid"),
        });
    }
    users
}

/// The scenario identities the forge needs: viewer, teammate, stranger, org.
#[allow(dead_code)]
pub struct Scenario {
    pub viewer: User,
    pub teammate: User,
    pub stranger: User,
    pub org: String,
}

pub fn build_scenario() -> Scenario {
    let users = generate_users(&mut Rng::from_label("users"), 12);
    let mut org_rng = Rng::from_label("org");
    let org = format!(
        "{}-{}-org",
        org_rng.pick(REPO_ADJECTIVES),
        org_rng.pick(REPO_NOUNS)
    );
    Scenario {
        viewer: users[0].clone(),
        teammate: users[1].clone(),
        stranger: users[2].clone(),
        org,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_matches_kit() {
        // Values produced by the kit's own buildScenario() (node, wire.ts).
        let scenario = build_scenario();
        assert_eq!(scenario.viewer.login, "adaiversen");
        assert_eq!(scenario.viewer.id, 1000);
        assert_eq!(scenario.viewer.name, "ada iversen");
        assert_eq!(scenario.viewer.email, "adaiversen@example.invalid");
        assert_eq!(scenario.teammate.login, "quinnkeller");
        assert_eq!(scenario.stranger.login, "olgabauer");
        assert_eq!(scenario.org, "faint-meadow-org");
    }

    #[test]
    fn rng_is_deterministic() {
        let mut first = Rng::from_label("users");
        let mut second = Rng::from_label("users");
        for _ in 0..1000 {
            assert_eq!(first.next_u64(), second.next_u64());
        }
    }
}

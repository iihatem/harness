pub struct Config {
    pub name: String,
    pub retries: u32,
}

impl Config {
    pub fn new(name: &str, retries: u32) -> Config {
        Config {
            name: name.to_string(),
            retries,
        }
    }
}

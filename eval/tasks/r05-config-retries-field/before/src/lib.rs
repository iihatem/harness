pub struct Config {
    pub name: String,
}

impl Config {
    pub fn new(name: &str) -> Config {
        Config {
            name: name.to_string(),
        }
    }
}

/// Reads the command line of the tools binary.
pub struct ArgParser {
    args: Vec<String>,
}

impl ArgParser {
    pub fn new(args: Vec<String>) -> Self {
        Self { args }
    }

    /// The long flags (`--name`) that were passed, in order.
    pub fn parse_flags(&self) -> Vec<String> {
        self.args
            .iter()
            .filter(|arg| arg.starts_with("--"))
            .cloned()
            .collect()
    }
}

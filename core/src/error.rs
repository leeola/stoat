use snafu::Snafu;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Node error: {message}"))]
    Node { message: String },

    #[snafu(display("General error: {message}"))]
    Generic { message: String },

    #[snafu(display("IO error: {message}"))]
    Io { message: String },

    #[snafu(display("Serialization error: {message}"))]
    Serialization { message: String },

    #[snafu(display("Unsupported operation '{operation}': {reason}"))]
    Unsupported { operation: String, reason: String },
}

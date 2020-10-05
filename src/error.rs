use serenity::framework::standard::ArgError;

#[derive(Debug)]
pub struct StringError(pub String);

impl std::fmt::Display for StringError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}", &self.0)
  }
}

impl std::error::Error for StringError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    None
  }
}

impl From<&str> for StringError {
  fn from(data: &str) -> Self {
    StringError(data.to_string())
  }
}

impl<T> From<ArgError<T>> for StringError
where
  T: std::fmt::Display,
{
  fn from(arg_error: ArgError<T>) -> Self {
    StringError(match arg_error {
      ArgError::Eos => String::from("Not enough arguments provided"),
      ArgError::Parse(parse_error) => format!("Failed parsing an argument: {}", parse_error),
      _ => String::from("Unknown argument parsing error"),
    })
  }
}

impl From<serenity::Error> for StringError {
  fn from(error: serenity::Error) -> Self {
    StringError(format!("{}", error))
  }
}

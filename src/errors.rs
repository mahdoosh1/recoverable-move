// Copyright (C) 2026 mahdoosh1
//
// This file is part of recoverable-move.
//
// recoverable-move is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 2 of the License, or
// (at your option) any later version.
//
// recoverable-move is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
// See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with recoverable-move. If not, see <https://www.gnu.org/licenses/>.

use std::fmt::Display;
#[derive(PartialEq)]
pub enum Error {
    IoError(String),
    Invalid(String)
}

impl Error {
    /// Construct an invalid-request error.
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }

    /// Construct an inconsistent-on-disk-state error.
    pub fn ioerror(msg: impl Into<String>) -> Self {
        Self::IoError(msg.into())
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::ioerror(value.to_string())
    }
}

impl From<gptman::Error> for Error {
    fn from(value: gptman::Error) -> Self {
        Error::ioerror(value.to_string())
    }
}

impl From<&Error> for String {
    fn from(value: &Error) -> Self {
        match value {
            Error::IoError(msg) => format!("io error: {}", msg),
            Error::Invalid(msg) => format!("error: {}", msg),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from(self))
    }
}

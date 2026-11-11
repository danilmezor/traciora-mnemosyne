//! Public SDK call boundaries. Host operations remain explicit so getters,
//! arbitrary thrown values and reentrant console calls retain their VM identity.
use super::{parse_units_outcome, ConsoleWarning, Document};
use crate::yaml_value::Diagnostic;

/// Classification after the wrapper's unconditional `source.length` read.
#[derive(Clone, Debug)]
pub enum ParseKind {
    String(Vec<u16>),
    /// False, zero, negative zero, NaN and BigInt zero reach an empty lexer.
    Falsy,
    /// Truthy non-string primitives and every object, including boxed strings.
    NonString,
}
/// Read the property through the host VM; never perform String(input) coercion.
pub trait ParseArgument {
    type Error;
    fn read_length(&mut self) -> Result<(), Self::Error>;
    fn kind(&self) -> ParseKind;
}
/// Each call must freshly resolve global console and its warn member, calling
/// that function with console as receiver. Its return value is discarded.
pub trait Console<E> {
    fn warn(&mut self, argument: ConsoleWarning) -> Result<(), E>;
}
#[derive(Debug)]
pub enum CallError<E> {
    /// A getter/function throw is preserved as the original host value.
    Host(E),
    Yaml(Diagnostic),
}
/// The SDK receiver and additional arguments are unused by the pinned wrapper.
pub fn parse<A, C>(argument: &mut A, console: &mut C) -> Result<Document, CallError<A::Error>>
where
    A: ParseArgument,
    C: Console<A::Error>,
{
    argument.read_length().map_err(CallError::Host)?;
    let source = match argument.kind() {
        ParseKind::String(units) => units,
        ParseKind::Falsy => Vec::new(),
        ParseKind::NonString => {
            return Err(CallError::Yaml(Diagnostic::exception(
                "TypeError",
                "source is not a string",
            )))
        }
    };
    let outcome = parse_units_outcome(&source);
    for warning in outcome.warnings {
        console.warn(warning).map_err(CallError::Host)?;
    }
    outcome.result.map_err(CallError::Yaml)
}

/// Values after the authored host frontend has read live getters/toJSON in
/// source order. This is a conversion boundary, not a substitute for that code.
#[derive(Clone, Debug)]
pub enum StringifyValue {
    Undefined,
    Document(Document),
    /// Canonical base-ten spelling returned by the VM's BigInt primitive API.
    BigInt(String),
    Symbol,
    Function,
}
pub trait StringifyArgument {
    type Error;
    fn convert(&mut self) -> Result<StringifyValue, Self::Error>;
}
pub fn stringify<A: StringifyArgument>(
    argument: &mut A,
) -> Result<Option<Vec<u16>>, CallError<A::Error>> {
    let value = argument.convert().map_err(CallError::Host)?;
    match value {
        StringifyValue::Undefined => Ok(None),
        StringifyValue::Document(document) => document
            .stringify_units()
            .map(Some)
            .map_err(CallError::Yaml),
        StringifyValue::BigInt(decimal) => {
            let mut units: Vec<u16> = decimal.encode_utf16().collect();
            units.push(10);
            Ok(Some(units))
        }
        StringifyValue::Symbol | StringifyValue::Function => {
            let kind = if matches!(value, StringifyValue::Symbol) {
                "Symbol"
            } else {
                "Function"
            };
            Err(CallError::Yaml(Diagnostic::exception(
                "Error",
                &format!("Tag not resolved for {kind} value"),
            )))
        }
    }
}

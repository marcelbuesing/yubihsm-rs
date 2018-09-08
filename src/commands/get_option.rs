//! Get auditing options which have been configured on the device.
//!
//! <https://developers.yubico.com/YubiHSM2/Commands/Get_Option.html>

use super::{Command, Response};
use audit::*;
use serializers::deserialize;
use session::{Session, SessionError, SessionErrorKind::ProtocolError};
use {Adapter, CommandType};

/// Get the audit policy setting for a particular command
pub fn get_command_audit_option<A: Adapter>(
    session: &mut Session<A>,
    command: CommandType,
) -> Result<Option<AuditOption>, SessionError> {
    let command_audit_options = get_all_command_audit_options(session)?;
    Ok(command_audit_options
        .iter()
        .find(|opt| opt.command == command)
        .map(|opt| opt.audit))
}

/// Get the audit policy settings for all commands
pub fn get_all_command_audit_options<A>(
    session: &mut Session<A>,
) -> Result<Vec<AuditCommand>, SessionError>
where
    A: Adapter,
{
    let response = session.send_command(GetOptionCommand {
        tag: AuditTag::Command,
    })?;

    Ok(deserialize(&response.0)?)
}

/// Get the forced auditing global option: when enabled, the device will
/// refuse operations if the [log store] becomes full.
///
/// [log store]: https://developers.yubico.com/YubiHSM2/Concepts/Logs.html
pub fn get_force_audit_option<A: Adapter>(
    session: &mut Session<A>,
) -> Result<AuditOption, SessionError> {
    let response = session.send_command(GetOptionCommand {
        tag: AuditTag::Force,
    })?;

    ensure!(
        response.0.len() == 1,
        ProtocolError,
        "expected 1-byte response, got {}",
        response.0.len()
    );

    AuditOption::from_u8(response.0[0]).map_err(|e| err!(ProtocolError, e))
}

/// Request parameters for `commands::get_option`
#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct GetOptionCommand {
    /// Tag byte for `Force` vs `Command` options
    pub tag: AuditTag,
}

impl Command for GetOptionCommand {
    type ResponseType = GetOptionResponse;
}

/// Response from `commands::get_option`
#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct GetOptionResponse(pub(crate) Vec<u8>);

impl Response for GetOptionResponse {
    const COMMAND_TYPE: CommandType = CommandType::GetOption;
}
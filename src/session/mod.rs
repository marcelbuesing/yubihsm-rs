use subtle::ConstantTimeEq;

#[macro_use]
mod error;

pub use self::error::{SessionError, SessionErrorKind};
use super::{ObjectId, SessionId};
use commands::{self, CloseSessionCommand, Command};
use connector::{Connector, HttpConfig, HttpConnector, Status as ConnectorStatus};
use securechannel::{
    Challenge, Channel, CommandMessage, ResponseCode, ResponseMessage, StaticKeys,
};
use serializers::deserialize;

/// Salt value to use with PBKDF2 when deriving static keys from a password
pub const PBKDF2_SALT: &[u8] = b"Yubico";

/// Number of PBKDF2 iterations to perform when deriving static keys
pub const PBKDF2_ITERATIONS: usize = 10_000;

/// Status message returned from healthy connectors
const CONNECTOR_STATUS_OK: &str = "OK";

/// Encrypted session with the `YubiHSM2`.
/// A session is needed to perform any commands.
///
/// Sessions are eneric over `Connector` types in case a different one needs to
/// be swapped in, which is primarily useful for substituting the `MockHSM`.
///
/// Sessions are automatically closed on `Drop`, releasing `YubiHSM2` session
/// resources and wiping the ephemeral keys used to encrypt the session.
pub struct Session<C = HttpConnector>
where
    C: Connector,
{
    /// ID of this session
    id: SessionId,

    /// Encrypted channel to the HSM
    channel: Channel,

    /// Connector to send messages through
    connector: C,

    /// Optional cached static keys for reconnecting lost sessions
    // TODO: session reconnect support
    #[allow(dead_code)]
    static_keys: Option<StaticKeys>,
}

// Special casing these for HttpConnector is a bit of a hack in that default
// generics and static methods do not play well together, e.g.
//
// error[E0283]: type annotations required: cannot resolve `yubihsm::Connector`
//
// So we special case these for HttpConnector to make the API more ergonomic
impl Session<HttpConnector> {
    /// Open a new session to the HSM, authenticating with the given keypair
    pub fn create(
        connector_config: HttpConfig,
        auth_key_id: ObjectId,
        static_keys: StaticKeys,
        reconnect: bool,
    ) -> Result<Self, SessionError> {
        let connector_info = connector_config.to_string();
        let connector = HttpConnector::open(connector_config)?;
        let status = connector.status()?;

        if status.message != CONNECTOR_STATUS_OK {
            session_fail!(
                CreateFailed,
                "bad status response from {}: {}",
                connector_info,
                status.message
            );
        }

        Self::new(connector, auth_key_id, static_keys, reconnect)
    }

    /// Open a new session to the HSM, authenticating with a given password
    #[cfg(feature = "passwords")]
    pub fn create_from_password(
        connector_config: HttpConfig,
        auth_key_id: ObjectId,
        password: &str,
        reconnect: bool,
    ) -> Result<Self, SessionError> {
        Self::create(
            connector_config,
            auth_key_id,
            StaticKeys::derive_from_password(password.as_bytes(), PBKDF2_SALT, PBKDF2_ITERATIONS),
            reconnect,
        )
    }
}

impl<C: Connector> Session<C> {
    /// Create a new encrypted session using the given connector, YubiHSM2 auth key ID, and
    /// static identity keys
    pub fn new(
        connector: C,
        auth_key_id: ObjectId,
        static_keys: StaticKeys,
        reconnect: bool,
    ) -> Result<Self, SessionError> {
        let host_challenge = Challenge::random();

        let (session_id, session_response) =
            commands::create_session(&connector, auth_key_id, host_challenge)?;

        let channel = Channel::new(
            session_id,
            &static_keys,
            host_challenge,
            session_response.card_challenge,
        );

        if channel
            .card_cryptogram()
            .ct_eq(&session_response.card_cryptogram)
            .unwrap_u8() != 1
        {
            session_fail!(AuthFailed, "card cryptogram mismatch!");
        }

        let static_keys_option = if reconnect { Some(static_keys) } else { None };

        let mut session = Self {
            id: session_id,
            channel,
            connector,
            static_keys: static_keys_option,
        };

        session.authenticate()?;
        Ok(session)
    }

    /// Get the current session ID
    #[inline]
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Request current yubihsm-connector status
    pub fn connector_status(&mut self) -> Result<ConnectorStatus, SessionError> {
        self.connector.status().map_err(|e| e.into())
    }

    /// Authenticate the current session with the `YubiHSM2`
    fn authenticate(&mut self) -> Result<(), SessionError> {
        let command = self.channel.authenticate_session()?;
        let response = self.send_command(command)?;
        self.channel
            .finish_authenticate_session(&response)
            .map_err(|e| e.into())
    }

    /// Send a command message to the YubiHSM2 and parse the response
    /// POST /connector/api with a given command message
    fn send_command(&mut self, cmd: CommandMessage) -> Result<ResponseMessage, SessionError> {
        let cmd_type = cmd.command_type;
        let uuid = cmd.uuid;

        // TODO: handle reconnecting when sessions are lost
        let response_bytes = self.connector.send_command(uuid, cmd.into())?;
        let response = ResponseMessage::parse(response_bytes)?;

        if response.is_err() {
            session_fail!(ResponseError, "HSM error: {:?}", response.code);
        }

        if response.command().unwrap() != cmd_type {
            session_fail!(
                ProtocolError,
                "command type mismatch: expected {:?}, got {:?}",
                cmd_type,
                response.command().unwrap()
            );
        }

        Ok(response)
    }

    /// Encrypt a command and send it to the card, then authenticate and
    /// decrypt the response
    pub(crate) fn send_encrypted_command<T: Command>(
        &mut self,
        command: T,
    ) -> Result<T::ResponseType, SessionError> {
        let plaintext_cmd = command.into();
        let encrypted_cmd = self.channel.encrypt_command(plaintext_cmd)?;

        let encrypted_response = self.send_command(encrypted_cmd)?;
        let response = self.channel.decrypt_response(encrypted_response)?;

        if response.is_err() {
            // TODO: factor this into ResponseMessage or ResponseCode?
            let description = match response.code {
                ResponseCode::MemoryError => {
                    "general HSM error (e.g. bad command params, missing object)".to_owned()
                }
                other => format!("{:?}", other),
            };

            session_fail!(ResponseError, description);
        }

        if response.command().unwrap() != T::COMMAND_TYPE {
            session_fail!(
                ResponseError,
                "command type mismatch: expected {:?}, got {:?}",
                T::COMMAND_TYPE,
                response.command().unwrap()
            );
        }

        deserialize(response.data.as_ref()).map_err(|e| e.into())
    }
}

/// Close session automatically on drop
impl<C: Connector> Drop for Session<C> {
    fn drop(&mut self) {
        let err = self.send_encrypted_command(CloseSessionCommand {}).err();
        debug_assert_eq!(err.map(|e| e.kind()), None);
    }
}

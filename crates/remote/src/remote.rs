pub mod json_log;
mod managed_ssh_keys;
pub mod protocol;
pub mod proxy;
pub mod remote_client;
pub mod remote_identity;
mod transport;

pub use managed_ssh_keys::{
    ManagedSshKey, ManagedSshKeyDeploymentState, delete_local_managed_ssh_key,
    list_managed_ssh_keys, managed_ssh_key_directory, revoke_and_delete_managed_ssh_key,
};
#[cfg(target_os = "windows")]
pub use remote_client::OpenWslPath;
pub use remote_client::{
    CommandTemplate, ConnectionIdentifier, ConnectionState, Interactive, RemoteArch, RemoteClient,
    RemoteClientDelegate, RemoteClientEvent, RemoteConnection, RemoteConnectionOptions, RemoteOs,
    RemotePlatform, connect, has_active_connection,
};
pub use remote_identity::{
    RemoteConnectionIdentity, remote_connection_identity, same_remote_connection_identity,
};
pub use transport::docker::DockerConnectionOptions;
pub use transport::ssh::{SshConnectionOptions, SshPortForwardOption};
pub use transport::wsl::WslConnectionOptions;
#[cfg(target_os = "windows")]
pub use transport::wsl::wsl_path_to_windows_path;

#[cfg(any(test, feature = "test-support"))]
pub use transport::mock::{
    MockConnection, MockConnectionOptions, MockConnectionRegistry, MockDelegate,
};

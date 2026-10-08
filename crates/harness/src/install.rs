//! Symphony is installed separately; the desktop never installs other agents.
use crate::{CancellationToken, HarnessError};
use zeron_proto::HarnessId;

pub fn can_install(_id: HarnessId) -> bool {
    false
}
pub fn manual_command(id: HarnessId) -> Option<&'static str> {
    (id == HarnessId::Symphony).then_some("See docs/symphony.md for Symphony installation")
}
pub async fn install_harness(
    id: HarnessId,
    _cancel: CancellationToken,
) -> Result<(), HarnessError> {
    Err(HarnessError::NotInstalled(format!(
        "{id:?}: install Symphony separately; see docs/symphony.md"
    )))
}

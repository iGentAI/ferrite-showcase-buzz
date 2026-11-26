use ferrite_app::AppContext;

use crate::cells::BranchState;
use crate::error::{ProcessError, Result};
use crate::planes;

pub(super) async fn update(
    ctx: &AppContext,
    key: &[u8],
    mut change: impl FnMut(&mut BranchState) -> Result<()>,
) -> Result<BranchState> {
    for _ in 0..32 {
        let stored = planes::load_versioned::<BranchState>(ctx, key).await?;
        let (mut current, version) = stored
            .map_or((BranchState::default(), None), |(state, version)| {
                (state, Some(version))
            });
        let before = current.clone();
        change(&mut current)?;
        if version.is_some() && current == before {
            return Ok(current);
        }
        if planes::save_if_version(ctx, key, &current, version).await? {
            return Ok(current);
        }
    }
    Err(ProcessError::Storage(
        "branch state remained contended".into(),
    ))
}

pub(super) async fn save_commands(
    ctx: &AppContext,
    key: &[u8],
    before: &BranchState,
    after: &BranchState,
) -> Result<()> {
    update(ctx, key, |current| {
        for id in after.answered.difference(&before.answered) {
            current.mark_answered(id);
        }
        let rooms: std::collections::BTreeSet<&String> = before
            .command_walk
            .keys()
            .chain(after.command_walk.keys())
            .collect();
        for room in rooms {
            let old = before.command_walk.get(room);
            let new = after.command_walk.get(room);
            if old != new && current.command_walk.get(room) == old {
                match new {
                    Some(walk) => {
                        current.command_walk.insert(room.clone(), walk.clone());
                    }
                    None => {
                        current.command_walk.remove(room);
                    }
                }
            }
        }
        for retired in &before.retired {
            if !after.retired.contains(retired) {
                current.retired.retain(|room| room != retired);
            }
        }
        Ok(())
    })
    .await
    .map(|_| ())
}

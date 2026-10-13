use client::rubiks_account::RubiksAccount;
use fs::GitCredentials;
use gpui::{App, Context, WeakEntity, Window};
use notifications::status_toast::StatusToast;
use std::sync::Arc;
use ui::{Color, Icon, IconName, IconSize, SharedString};
use util::ResultExt;
use workspace::{self, Workspace};

fn show_error_toast(message: String, workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let toast = StatusToast::new(message, cx, |this, _| {
        this.icon(
            Icon::new(IconName::XCircle)
                .size(IconSize::Small)
                .color(Color::Error),
        )
        .dismiss_button(true)
    });
    workspace.toggle_status_toast(toast, cx);
}

/// Lists the user's repositories from the Rubiks gateway and clones the chosen one.
pub fn clone_from_rubiks(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(account) = RubiksAccount::global(cx) else {
        return;
    };
    let repositories = account.read(cx).repositories(cx);
    let credentials = account.read(cx).github_credentials(cx);
    let workspace = workspace.weak_handle();
    window
        .spawn(cx, async move |cx| {
            let repositories = match repositories.await {
                Ok(repositories) if repositories.is_empty() => {
                    Err(anyhow::anyhow!("no GitHub repositories found for your account"))
                }
                result => result,
            };
            let repositories = match repositories {
                Ok(repositories) => repositories,
                Err(error) => {
                    workspace
                        .update(cx, |workspace, cx| {
                            show_error_toast(format!("Rubiks: {error:#}"), workspace, cx)
                        })
                        .log_err();
                    return None;
                }
            };

            let options = repositories
                .iter()
                .map(|repository| {
                    if repository.private {
                        SharedString::from(format!("{} (private)", repository.full_name))
                    } else {
                        SharedString::from(repository.full_name.clone())
                    }
                })
                .collect();
            let selection = cx
                .update(|window, cx| {
                    crate::picker_prompt::prompt(
                        "Clone a repository",
                        options,
                        workspace.clone(),
                        window,
                        cx,
                    )
                })
                .ok()?;
            let repository = repositories.get(selection.await?)?;
            // Without them, public repositories still clone.
            let credentials = credentials.await.log_err();

            cx.update(|window, cx| {
                clone_and_open_with_credentials(
                    repository.clone_url.clone().into(),
                    credentials,
                    workspace,
                    window,
                    cx,
                    Arc::new(|_workspace: &mut Workspace, _window, _cx| {}),
                )
            })
            .ok()
        })
        .detach();
}

pub fn clone_and_open(
    repo_url: SharedString,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
    on_success: Arc<
        dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + Send + Sync + 'static,
    >,
) {
    clone_and_open_with_credentials(repo_url, None, workspace, window, cx, on_success)
}

pub fn clone_and_open_with_credentials(
    repo_url: SharedString,
    credentials: Option<GitCredentials>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
    on_success: Arc<
        dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + Send + Sync + 'static,
    >,
) {
    let destination_prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Select as Repository Destination".into()),
    });

    window
        .spawn(cx, async move |cx| {
            let mut paths = destination_prompt.await.ok()?.ok()??;
            let mut destination_dir = paths.pop()?;

            let repo_name = repo_url
                .split('/')
                .next_back()
                .map(|name| name.strip_suffix(".git").unwrap_or(name))
                .unwrap_or("repository")
                .to_owned();

            let clone_task = workspace
                .update(cx, |workspace, cx| {
                    let fs = workspace.app_state().fs.clone();
                    let destination_dir = destination_dir.clone();
                    let repo_url = repo_url.clone();
                    cx.spawn(async move |_workspace, _cx| {
                        fs.git_clone(destination_dir.as_path(), &repo_url, credentials)
                            .await
                    })
                })
                .ok()?;

            if let Err(error) = clone_task.await {
                workspace
                    .update(cx, |workspace, cx| {
                        show_error_toast(error.to_string(), workspace, cx)
                    })
                    .log_err();
                return None;
            }

            let has_worktrees = workspace
                .read_with(cx, |workspace, cx| {
                    workspace.project().read(cx).worktrees(cx).next().is_some()
                })
                .ok()?;

            let prompt_answer = if has_worktrees {
                cx.update(|window, cx| {
                    window.prompt(
                        gpui::PromptLevel::Info,
                        &format!("Git Clone: {}", repo_name),
                        None,
                        &["Add repo to project", "Open repo in new project"],
                        cx,
                    )
                })
                .ok()?
                .await
                .ok()?
            } else {
                // Don't ask if project is empty
                0
            };

            destination_dir.push(&repo_name);

            match prompt_answer {
                0 => {
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            let create_task = workspace.project().update(cx, |project, cx| {
                                project.create_worktree(destination_dir.as_path(), true, cx)
                            });

                            let workspace_weak = cx.weak_entity();
                            let on_success = on_success.clone();
                            cx.spawn_in(window, async move |_window, cx| {
                                if create_task.await.log_err().is_some() {
                                    workspace_weak
                                        .update_in(cx, |workspace, window, cx| {
                                            (on_success)(workspace, window, cx);
                                        })
                                        .ok();
                                }
                            })
                            .detach();
                        })
                        .ok()?;
                }
                1 => {
                    workspace
                        .update(cx, move |workspace, cx| {
                            let app_state = workspace.app_state().clone();
                            let destination_path = destination_dir.clone();
                            let on_success = on_success.clone();

                            workspace::open_new(
                                Default::default(),
                                app_state,
                                cx,
                                move |workspace, window, cx| {
                                    cx.activate(true);

                                    let create_task =
                                        workspace.project().update(cx, |project, cx| {
                                            project.create_worktree(
                                                destination_path.as_path(),
                                                true,
                                                cx,
                                            )
                                        });

                                    let workspace_weak = cx.weak_entity();
                                    cx.spawn_in(window, async move |_window, cx| {
                                        if create_task.await.log_err().is_some() {
                                            workspace_weak
                                                .update_in(cx, |workspace, window, cx| {
                                                    (on_success)(workspace, window, cx);
                                                })
                                                .ok();
                                        }
                                    })
                                    .detach();
                                },
                            )
                            .detach();
                        })
                        .ok();
                }
                _ => {}
            }

            Some(())
        })
        .detach();
}

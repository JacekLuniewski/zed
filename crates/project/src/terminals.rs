use crate::Project;
use anyhow::Context as _;
use collections::HashMap;
use futures::channel::mpsc::UnboundedSender;
use gpui::{AnyWindowHandle, Context, Entity, Model, ModelContext, WeakModel};
use rpc::proto;
use settings::{Settings, SettingsLocation};
use smol::channel::bounded;
use std::{
    ops::ControlFlow,
    path::{Path, PathBuf},
};
use task::SpawnInTerminal;
use terminal::{
    terminal_settings::{self, Shell, TerminalSettings, VenvSettingsContent},
    RemotePty, TaskState, TaskStatus, Terminal, TerminalBuilder,
};
use util::ResultExt;

// #[cfg(target_os = "macos")]
// use std::os::unix::ffi::OsStrExt;

pub struct Terminals {
    pub(crate) local_handles: Vec<WeakModel<terminal::Terminal>>,
    pub(crate) remote_handles: HashMap<u64, UnboundedSender<Vec<u8>>>,
}

impl Project {
    pub fn create_terminal(
        &mut self,
        working_directory: Option<PathBuf>,
        spawn_task: Option<SpawnInTerminal>,
        window: AnyWindowHandle,
        cx: &mut ModelContext<Self>,
    ) -> anyhow::Result<Model<Terminal>> {
        // TODO kb only do that for remote projects where I am the owner
        if self.is_remote() {
            let new_terminal_id = if true { todo!("TODO kb ") } else { 0 };
            let client = self.client();
            let project_id = self
                .remote_id()
                .context("remote project without a remote id")?;
            let (remote_pty, host_tx) = RemotePty::new(
                move |data| {
                    let client = client.clone();
                    async move {
                        match client
                            // TODO kb instead, have a streaming response to avoid sending many messages with bytes
                            .request(proto::InputRemoteTerminal {
                                project_id,
                                terminal_id: new_terminal_id,
                                data,
                            })
                            .await
                            .log_err()
                        {
                            Some(_response) => {
                                // TODO kb return a cancellation here?
                                ControlFlow::Continue(())
                            }
                            None => ControlFlow::Break(()),
                        }
                    }
                },
                cx,
            )
            .context("remote pty creation")?;
            self.terminals
                .remote_handles
                .insert(new_terminal_id, host_tx);
            todo!("TODO kb")
        } else {
            // used only for TerminalSettings::get
            let worktree = {
                let terminal_cwd = working_directory.as_deref();
                let task_cwd = spawn_task
                    .as_ref()
                    .and_then(|spawn_task| spawn_task.cwd.as_deref());

                terminal_cwd
                    .and_then(|terminal_cwd| self.find_local_worktree(terminal_cwd, cx))
                    .or_else(|| {
                        task_cwd.and_then(|spawn_cwd| self.find_local_worktree(spawn_cwd, cx))
                    })
            };

            let settings_location = worktree.as_ref().map(|(worktree, path)| SettingsLocation {
                worktree_id: worktree.read(cx).id().to_usize(),
                path,
            });

            let is_terminal = spawn_task.is_none();
            let settings = TerminalSettings::get(settings_location, cx);
            let python_settings = settings.detect_venv.clone();
            let (completion_tx, completion_rx) = bounded(1);

            let mut env = settings.env.clone();
            // Alacritty uses parent project's working directory when no working directory is provided
            // https://github.com/alacritty/alacritty/blob/fd1a3cc79192d1d03839f0fd8c72e1f8d0fce42e/extra/man/alacritty.5.scd?plain=1#L47-L52

            let venv_base_directory = working_directory
                .as_deref()
                .unwrap_or_else(|| Path::new(""));

            let (spawn_task, shell) = if let Some(spawn_task) = spawn_task {
                log::debug!("Spawning task: {spawn_task:?}");
                env.extend(spawn_task.env);
                // Activate minimal Python virtual environment
                if let Some(python_settings) = &python_settings.as_option() {
                    self.set_python_venv_path_for_tasks(
                        python_settings,
                        venv_base_directory,
                        &mut env,
                    );
                }
                (
                    Some(TaskState {
                        id: spawn_task.id,
                        full_label: spawn_task.full_label,
                        label: spawn_task.label,
                        command_label: spawn_task.command_label,
                        status: TaskStatus::Running,
                        completion_rx,
                    }),
                    Shell::WithArguments {
                        program: spawn_task.command,
                        args: spawn_task.args,
                    },
                )
            } else {
                (None, settings.shell.clone())
            };

            let terminal = TerminalBuilder::new(
                working_directory.clone(),
                spawn_task,
                shell,
                env,
                Some(settings.blinking.clone()),
                settings.alternate_scroll,
                settings.max_scroll_history_lines,
                window,
                completion_tx,
            )
            .map(|builder| {
                let terminal_handle = cx.new_model(|cx| builder.subscribe(cx));

                self.terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;

                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                // if the terminal is not a task, activate full Python virtual environment
                if is_terminal {
                    if let Some(python_settings) = &python_settings.as_option() {
                        if let Some(activate_script_path) =
                            self.find_activate_script_path(python_settings, venv_base_directory)
                        {
                            self.activate_python_virtual_environment(
                                Project::get_activate_command(python_settings),
                                activate_script_path,
                                &terminal_handle,
                                cx,
                            );
                        }
                    }
                }
                terminal_handle
            });

            terminal
        }
    }

    pub fn find_activate_script_path(
        &mut self,
        settings: &VenvSettingsContent,
        venv_base_directory: &Path,
    ) -> Option<PathBuf> {
        let activate_script_name = match settings.activate_script {
            terminal_settings::ActivateScript::Default => "activate",
            terminal_settings::ActivateScript::Csh => "activate.csh",
            terminal_settings::ActivateScript::Fish => "activate.fish",
            terminal_settings::ActivateScript::Nushell => "activate.nu",
        };

        settings
            .directories
            .into_iter()
            .find_map(|virtual_environment_name| {
                let path = venv_base_directory
                    .join(virtual_environment_name)
                    .join("bin")
                    .join(activate_script_name);
                path.exists().then_some(path)
            })
    }

    pub fn set_python_venv_path_for_tasks(
        &mut self,
        settings: &VenvSettingsContent,
        venv_base_directory: &Path,
        env: &mut HashMap<String, String>,
    ) {
        let activate_path = settings
            .directories
            .into_iter()
            .find_map(|virtual_environment_name| {
                let path = venv_base_directory.join(virtual_environment_name);
                path.exists().then_some(path)
            });

        if let Some(path) = activate_path {
            // Some tools use VIRTUAL_ENV to detect the virtual environment
            env.insert(
                "VIRTUAL_ENV".to_string(),
                path.to_string_lossy().to_string(),
            );

            let path_bin = path.join("bin");
            // We need to set the PATH to include the virtual environment's bin directory
            if let Some(paths) = std::env::var_os("PATH") {
                let paths = std::iter::once(path_bin).chain(std::env::split_paths(&paths));
                if let Some(new_path) = std::env::join_paths(paths).log_err() {
                    env.insert("PATH".to_string(), new_path.to_string_lossy().to_string());
                }
            } else {
                env.insert(
                    "PATH".to_string(),
                    path.join("bin").to_string_lossy().to_string(),
                );
            }
        }
    }

    fn get_activate_command(settings: &VenvSettingsContent) -> &'static str {
        match settings.activate_script {
            terminal_settings::ActivateScript::Nushell => "overlay use",
            _ => "source",
        }
    }

    fn activate_python_virtual_environment(
        &mut self,
        activate_command: &'static str,
        activate_script: PathBuf,
        terminal_handle: &Model<Terminal>,
        cx: &mut ModelContext<Project>,
    ) {
        // Paths are not strings so we need to jump through some hoops to format the command without `format!`
        let mut command = Vec::from(activate_command.as_bytes());
        command.push(b' ');
        // Wrapping path in double quotes to catch spaces in folder name
        command.extend_from_slice(b"\"");
        command.extend_from_slice(activate_script.as_os_str().as_encoded_bytes());
        command.extend_from_slice(b"\"");
        command.push(b'\n');

        terminal_handle.update(cx, |this, _| this.input_bytes(command));
    }
}

// TODO: Add a few tests for adding and removing terminal tabs

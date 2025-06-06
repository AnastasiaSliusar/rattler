use create_shortcut::Shortcut;
use fs_err as fs;
use rattler_conda_types::{
    menuinst::{WindowsFileExtension, WindowsTerminalProfile, WindowsTracker, WindowsUrlProtocol},
    Platform,
};
use rattler_shell::{
    activation::{ActivationVariables, Activator, PathModificationBehavior},
    shell,
};
use registry::{notify_shell_changes, FileExtension, UrlProtocol};
use std::{
    io::Write as _,
    path::{Path, PathBuf},
};
use terminal::TerminalProfile;
pub use terminal::TerminalUpdateError;

use crate::{
    render::{BaseMenuItemPlaceholders, MenuItemPlaceholders},
    schema::{Environment, MenuItemCommand, Windows},
    utils::{log_output, slugify},
    MenuInstError, MenuMode,
};

mod create_shortcut;
mod knownfolders;
mod lex;
mod registry;
mod terminal;

use knownfolders::{Folder, UserHandle};

pub struct Directories {
    start_menu: PathBuf,
    quick_launch: Option<PathBuf>,
    desktop: PathBuf,
    windows_terminal_settings_files: Vec<PathBuf>,
}

/// On Windows we can create shortcuts in several places:
/// - Start Menu
/// - Desktop
/// - Quick launch (only for user installs)
impl Directories {
    pub fn create(menu_mode: MenuMode) -> Directories {
        let user_handle = match menu_mode {
            MenuMode::System => UserHandle::Common,
            MenuMode::User => UserHandle::Current,
        };

        let known_folders = knownfolders::Folders::new();
        let start_menu = known_folders
            .get_folder_path(Folder::Start, user_handle)
            .unwrap();
        let quick_launch = if menu_mode == MenuMode::User {
            known_folders
                .get_folder_path(Folder::QuickLaunch, user_handle)
                .ok()
        } else {
            None
        };
        let desktop = known_folders
            .get_folder_path(Folder::Desktop, user_handle)
            .unwrap();

        let windows_terminal_settings_files =
            terminal::windows_terminal_settings_files(menu_mode, &known_folders);

        Directories {
            start_menu,
            quick_launch,
            desktop,
            windows_terminal_settings_files,
        }
    }

    /// Create a fake Directories struct for testing ONLY
    pub fn fake_folders(path: &Path) -> Directories {
        // Prepare the directories
        fs::create_dir_all(path).unwrap();

        let terminal_settings_json = path.join("terminal_settings.json");
        if !terminal_settings_json.exists() {
            // This is for testing only, so we can ignore the result
            fs::write(&terminal_settings_json, "{}").unwrap();
        }

        let start_menu = path.join("Start Menu");
        fs::create_dir_all(&start_menu).unwrap();

        let quick_launch = Some(path.join("Quick Launch"));
        fs::create_dir_all(quick_launch.as_ref().unwrap()).unwrap();

        let desktop = path.join("Desktop");
        fs::create_dir_all(&desktop).unwrap();

        Directories {
            start_menu,
            quick_launch,
            desktop,
            windows_terminal_settings_files: vec![terminal_settings_json],
        }
    }
}

pub struct WindowsMenu {
    menu_name: String,
    prefix: PathBuf,
    name: String,
    item: Windows,
    command: MenuItemCommand,
    directories: Directories,
    placeholders: MenuItemPlaceholders,
    menu_mode: MenuMode,
}

const SHORTCUT_EXTENSION: &str = "lnk";

impl WindowsMenu {
    pub fn new(
        menu_name: &str,
        prefix: &Path,
        item: Windows,
        command: MenuItemCommand,
        directories: Directories,
        placeholders: &BaseMenuItemPlaceholders,
        menu_mode: MenuMode,
    ) -> Self {
        let name = command.name.resolve(Environment::Base, placeholders);

        let shortcut_name = format!("{name}.{SHORTCUT_EXTENSION}");

        let location = directories
            .start_menu
            .join(&shortcut_name)
            .with_extension(SHORTCUT_EXTENSION);

        Self {
            menu_name: menu_name.to_string(),
            prefix: prefix.to_path_buf(),
            name,
            item,
            command,
            directories,
            placeholders: placeholders.refine(&location),
            menu_mode,
        }
    }

    fn script_content(&self) -> Result<String, MenuInstError> {
        let mut lines = vec![
            "@echo off".to_string(),
            ":: Script generated by conda/menuinst".to_string(),
        ];

        if let Some(pre_command_code) = self.command.precommand.as_ref() {
            lines.push(pre_command_code.resolve(&self.placeholders));
        }

        if self.command.activate.unwrap_or_default() {
            // create a bash activation script and emit it into the script
            let activator =
                Activator::from_path(&self.prefix, shell::CmdExe, Platform::current()).unwrap();
            let activation_variables = ActivationVariables {
                path_modification_behavior: PathModificationBehavior::Prepend,
                ..Default::default()
            };
            let activation_env = activator.run_activation(activation_variables, None)?;

            for (k, v) in activation_env {
                lines.push(format!(r#"set "{k}={v}""#));
            }
        }

        let args: Vec<String> = self
            .command
            .command
            .iter()
            .map(|elem| elem.resolve(&self.placeholders))
            .collect();

        lines.push(lex::quote_args(&args).join(" "));

        Ok(lines.join("\n"))
    }

    fn write_script(&self, path: &Path) -> Result<(), MenuInstError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = fs::File::create(path)?;
        file.write_all(self.script_content()?.as_bytes())?;

        Ok(())
    }

    fn path_for_script(&self) -> PathBuf {
        self.prefix.join("Menu").join(format!("{}.bat", &self.name))
    }

    fn build_command(&self, with_arg1: bool) -> Result<Vec<String>, MenuInstError> {
        if self.command.activate.unwrap_or(false) {
            let script_path = self.path_for_script();
            self.write_script(&script_path)?;

            let system_root = std::env::var("SystemRoot").unwrap_or("C:\\Windows".to_string());
            let system32 = Path::new(&system_root).join("system32");
            let cmd_exe = system32.join("cmd.exe").to_string_lossy().to_string();

            if self.command.terminal.unwrap_or(false) {
                let mut command = [&cmd_exe, "/D", "/K"]
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect::<Vec<String>>();

                // add script path with quotes
                command.push(format!("\"{}\"", script_path.to_string_lossy()));

                if with_arg1 {
                    command.push("%1".to_string());
                }
                Ok(command)
            } else {
                let script_path = self.path_for_script();
                self.write_script(&script_path)?;

                let arg1 = if with_arg1 { "%1 " } else { "" };
                let powershell = system32
                    .join("WindowsPowerShell")
                    .join("v1.0")
                    .join("powershell.exe")
                    .to_string_lossy()
                    .to_string();

                let mut command = [
                    &cmd_exe,
                    "/D",
                    "/C",
                    "START",
                    "/MIN",
                    "\"\"",
                    &powershell,
                    "-WindowStyle",
                    "hidden",
                ]
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>();

                command.push(format!(
                    "\"start '{}' {}-WindowStyle hidden\"",
                    script_path.to_string_lossy(),
                    arg1
                ));

                Ok(command)
            }
        } else {
            let mut command = Vec::new();
            for elem in self.command.command.iter() {
                command.push(elem.resolve(&self.placeholders));
            }

            if with_arg1 && !command.iter().any(|s| s.contains("%1")) {
                command.push("%1".to_string());
            }

            Ok(command)
        }
    }

    fn precreate(&self) -> Result<(), MenuInstError> {
        if let Some(precreate_code) = self.command.precreate.as_ref() {
            let precreate_code = precreate_code.resolve(&self.placeholders);

            if precreate_code.is_empty() {
                return Ok(());
            }

            let mut temp_file = tempfile::NamedTempFile::with_suffix(".bat")?;
            temp_file.write_all(precreate_code.as_bytes())?;

            // Close file and keep temporary path around
            let path = temp_file.into_temp_path();

            let output = std::process::Command::new("cmd")
                .arg("/c")
                .arg(&path)
                .output()?;

            log_output("precreate", output);
        }
        Ok(())
    }

    fn app_id(&self) -> String {
        match self.item.app_user_model_id.as_ref() {
            Some(aumi) => aumi.resolve(&self.placeholders),
            None => format!(
                "Menuinst.{}",
                slugify(&self.name)
                    .replace(".", "")
                    .chars()
                    .take(128)
                    .collect::<String>()
            ),
        }
    }

    fn create_shortcut(
        &self,
        args: &[String],
        tracker: &mut WindowsTracker,
    ) -> Result<(), MenuInstError> {
        let icon = self
            .command
            .icon
            .as_ref()
            .map(|s| s.resolve(&self.placeholders));

        let workdir = if let Some(workdir) = &self.command.working_dir {
            workdir.resolve(&self.placeholders)
        } else {
            "%HOMEPATH%".to_string()
        };

        if workdir != "%HOMEPATH%" {
            fs::create_dir_all(&workdir)?;
        }

        let app_id = self.app_id();

        // split args into command and arguments
        let Some((command, args)) = args.split_first() else {
            return Ok(());
        };
        let args = lex::quote_args(args).join(" ");

        let link_name = format!("{}.lnk", self.name);

        // install start menu shortcut
        let start_menu_subdir_path = self.directories.start_menu.join(&self.menu_name);
        if !start_menu_subdir_path.exists() {
            fs::create_dir_all(&start_menu_subdir_path)?;
            tracker.start_menu_subdir_path = Some(start_menu_subdir_path.clone());
        }

        let start_menu_link_path = start_menu_subdir_path.join(&link_name);
        let shortcut = Shortcut {
            path: command,
            description: &self.command.description.resolve(&self.placeholders),
            filename: &start_menu_link_path,
            arguments: Some(&args),
            workdir: Some(&workdir),
            iconpath: icon.as_deref(),
            iconindex: Some(0),
            app_id: Some(&app_id),
        };
        create_shortcut::create_shortcut(shortcut)?;
        tracker.shortcuts.push(start_menu_link_path.clone());

        // install desktop shortcut
        if self.item.desktop.unwrap_or(true) {
            let desktop_link_path = self.directories.desktop.join(&link_name);
            let shortcut = Shortcut {
                path: command,
                description: &self.command.description.resolve(&self.placeholders),
                filename: &desktop_link_path,
                arguments: Some(&args),
                workdir: Some(&workdir),
                iconpath: icon.as_deref(),
                iconindex: Some(0),
                app_id: Some(&app_id),
            };

            create_shortcut::create_shortcut(shortcut)?;
            tracker.shortcuts.push(desktop_link_path.clone());
        }

        // install quicklaunch shortcut
        if let Some(quick_launch_dir) = self.directories.quick_launch.as_ref() {
            if self.item.quicklaunch.unwrap_or(false) {
                let quicklaunch_link_path = quick_launch_dir.join(link_name);
                let shortcut = Shortcut {
                    path: command,
                    description: &self.command.description.resolve(&self.placeholders),
                    filename: &quicklaunch_link_path,
                    arguments: Some(&args),
                    workdir: Some(&workdir),
                    iconpath: icon.as_deref(),
                    iconindex: Some(0),
                    app_id: Some(&app_id),
                };

                create_shortcut::create_shortcut(shortcut)?;
                tracker.shortcuts.push(quicklaunch_link_path.clone());
            }
        }
        Ok(())
    }

    fn icon(&self) -> Option<String> {
        self.command
            .icon
            .as_ref()
            .map(|s| s.resolve(&self.placeholders))
    }

    fn register_file_extensions(&self, tracker: &mut WindowsTracker) -> Result<(), MenuInstError> {
        let Some(extensions) = &self.item.file_extensions else {
            return Ok(());
        };

        let icon = self.icon();
        let command = self.build_command(true)?.join(" ");
        let name = &self.name;
        let app_user_model_id = self.app_id();

        for extension in extensions {
            let extension = extension.resolve(&self.placeholders);
            let identifier = format!("{name}.AssocFile{extension}");
            let file_extension = FileExtension {
                extension: &extension,
                identifier: &identifier,
                command: &command,
                icon: icon.as_deref(),
                app_name: Some(name),
                app_user_model_id: Some(&app_user_model_id),
                friendly_type_name: None,
            };

            registry::register_file_extension(file_extension, self.menu_mode)?;

            tracker.file_extensions.push(WindowsFileExtension {
                extension: extension.clone(),
                identifier: identifier.clone(),
            });
        }

        Ok(())
    }

    fn register_url_protocols(&self, tracker: &mut WindowsTracker) -> Result<bool, MenuInstError> {
        let protocols = match &self.item.url_protocols {
            Some(protocols) if !protocols.is_empty() => protocols,
            _ => return Ok(false),
        };

        let command = self.build_command(true)?.join(" ");
        let icon = self.icon();
        let name = &self.name;
        let app_user_model_id = format!("{name}.Protocol");

        for protocol in protocols {
            let protocol = protocol.resolve(&self.placeholders);
            let identifier = format!("{name}.Protocol{protocol}");

            let url_protocol = UrlProtocol {
                protocol: &protocol,
                command: &command,
                identifier: &identifier,
                icon: icon.as_deref(),
                app_name: Some(name),
                app_user_model_id: Some(&app_user_model_id),
            };

            registry::register_url_protocol(url_protocol, self.menu_mode)?;
            tracker.url_protocols.push(WindowsUrlProtocol {
                protocol: protocol.clone(),
                identifier: identifier.clone(),
            });
        }

        Ok(true)
    }

    fn register_windows_terminal(&self, tracker: &mut WindowsTracker) -> Result<(), MenuInstError> {
        let Some(terminal_profile) = self.item.terminal_profile.as_ref() else {
            return Ok(());
        };

        let terminal_profile = terminal_profile.resolve(&self.placeholders);

        let profile = TerminalProfile {
            name: terminal_profile,
            icon: self.icon(),
            commandline: self.build_command(true)?.join(" "),
            starting_directory: self
                .command
                .working_dir
                .as_ref()
                .map(|s| s.resolve(&self.placeholders)),
        };

        for location in &self.directories.windows_terminal_settings_files {
            terminal::add_windows_terminal_profile(location, &profile)?;
            tracker.terminal_profiles.push(WindowsTerminalProfile {
                configuration_file: location.clone(),
                identifier: profile.name.clone(),
            });
        }

        Ok(())
    }

    pub fn install(&self, tracker: &mut WindowsTracker) -> Result<(), MenuInstError> {
        let args = self.build_command(false)?;
        self.precreate()?;
        self.create_shortcut(&args, tracker)?;
        self.register_file_extensions(tracker)?;
        self.register_url_protocols(tracker)?;
        self.register_windows_terminal(tracker)?;
        notify_shell_changes();
        Ok(())
    }
}

pub(crate) fn install_menu_item(
    menu_name: &str,
    prefix: &Path,
    windows_item: Windows,
    command: MenuItemCommand,
    placeholders: &BaseMenuItemPlaceholders,
    menu_mode: MenuMode,
) -> Result<WindowsTracker, MenuInstError> {
    let mut tracker = WindowsTracker::new(menu_mode);
    let directories = if let Ok(fake_dirs) = std::env::var("MENUINST_FAKE_DIRECTORIES") {
        Directories::fake_folders(Path::new(&fake_dirs))
    } else {
        Directories::create(menu_mode)
    };

    let menu = WindowsMenu::new(
        menu_name,
        prefix,
        windows_item,
        command,
        directories,
        placeholders,
        menu_mode,
    );
    menu.install(&mut tracker)?;
    Ok(tracker)
}

pub(crate) fn remove_menu_item(tracker: &WindowsTracker) -> Result<(), MenuInstError> {
    for file in &tracker.shortcuts {
        match fs::remove_file(file) {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("Failed to remove shortcut {}: {}", file.display(), e);
            }
        }
    }

    if let Some(subdir) = &tracker.start_menu_subdir_path {
        // Check if subdir exists and is empty.
        if subdir.exists() && subdir.read_dir()?.next().is_none() {
            match fs::remove_dir(subdir) {
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        "Failed to remove start menu sub-directory {}: {}",
                        subdir.display(),
                        e
                    );
                }
            }
        }
    }

    let menu_mode = tracker.menu_mode;
    for ext in &tracker.file_extensions {
        match registry::unregister_file_extension(&ext.extension, &ext.identifier, menu_mode) {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    "Failed to remove file extension {} with identifier {}: {}",
                    ext.extension,
                    ext.identifier,
                    e
                );
            }
        }
    }

    for protocol in &tracker.url_protocols {
        match registry::unregister_url_protocol(&protocol.protocol, &protocol.identifier, menu_mode)
        {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    "Failed to remove URL protocol {} with identifier {}: {}",
                    protocol.protocol,
                    protocol.identifier,
                    e
                );
            }
        }
    }

    for profile in &tracker.terminal_profiles {
        match terminal::remove_terminal_profile(&profile.configuration_file, &profile.identifier) {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    "Failed to remove terminal profile {} in {}: {}",
                    profile.identifier,
                    profile.configuration_file.display(),
                    e
                );
            }
        }
    }

    notify_shell_changes();
    Ok(())
}

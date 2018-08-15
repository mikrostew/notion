use semver::VersionReq;

use notion_core::serial::version::parse_requirements;
use notion_core::session::{ActivityKind, Session};
use notion_fail::Fallible;

use Notion;
use command::{Command, CommandName, Help};

#[derive(Debug, Deserialize)]
pub(crate) struct Args {
    arg_tool: String,
    arg_version: String,
}

pub(crate) enum Install {
    Help,
    Node(VersionReq),
    Yarn(VersionReq),
    Other { name: String, version: VersionReq },
}

impl Command for Install {
    type Args = Args;

    const USAGE: &'static str = "
Install a tool in the user toolchain

Usage:
    notion install <tool> <version>
    notion install -h | --help

Options:
    -h, --help     Display this message
";

    fn help() -> Self {
        Install::Help
    }

    fn parse(
        _: Notion,
        Args {
            arg_tool,
            arg_version,
        }: Args,
    ) -> Fallible<Self> {
        match &arg_tool[..] {
            "node" => Ok(Install::Node(parse_requirements(&arg_version)?)),
            "yarn" => Ok(Install::Yarn(parse_requirements(&arg_version)?)),
            ref tool => Ok(Install::Other {
                name: tool.to_string(),
                version: parse_requirements(&arg_version)?,
            }),
        }
    }

    fn run(self, session: &mut Session) -> Fallible<bool> {
        session.add_event_start(ActivityKind::Install);
        match self {
            Install::Help => {
                Help::Command(CommandName::Install).run(session)?;
            }
            Install::Node(requirements) => {
                session.set_default_node(&requirements)?;
            }
            Install::Yarn(requirements) => {
                session.set_default_yarn(&requirements)?;
            }
            Install::Other {
                name: _,
                version: _,
            } => unimplemented!(),
        };
        session.add_event_end(ActivityKind::Install, 0);
        Ok(true)
    }
}

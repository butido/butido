use std::fmt::Display;
use std::path::PathBuf;
use std::process::Command;

use clap_v3 as clap;
use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use clap::ArgMatches;
use diesel::ExpressionMethods;
use diesel::JoinOnDsl;
use diesel::QueryDsl;
use diesel::RunQueryDsl;
use itertools::Itertools;
use syntect::easy::HighlightLines;
use syntect::highlighting::{ThemeSet, Style};
use syntect::parsing::SyntaxSet;
use syntect::util::{as_24_bit_terminal_escaped, LinesWithEndings};

use crate::config::Configuration;
use crate::db::DbConnectionConfig;
use crate::db::models;
use crate::log::LogItem;

pub fn db<'a>(db_connection_config: DbConnectionConfig, config: &Configuration<'a>, matches: &ArgMatches) -> Result<()> {
    match matches.subcommand() {
        ("cli", Some(matches))        => cli(db_connection_config, matches),
        ("artifacts", Some(matches))  => artifacts(db_connection_config, matches),
        ("envvars", Some(matches))    => envvars(db_connection_config, matches),
        ("images", Some(matches))     => images(db_connection_config, matches),
        ("submits", Some(matches))    => submits(db_connection_config, matches),
        ("jobs", Some(matches))       => jobs(db_connection_config, matches),
        ("job", Some(matches))        => job(db_connection_config, config, matches),
        (other, _) => return Err(anyhow!("Unknown subcommand: {}", other)),
    }
}

fn cli(db_connection_config: DbConnectionConfig, matches: &ArgMatches) -> Result<()> {
    trait PgCliCommand {
        fn run_for_uri(&self, dbcc: DbConnectionConfig)  -> Result<()>;
    }

    struct Psql(PathBuf);
    impl PgCliCommand for Psql {
        fn run_for_uri(&self, dbcc: DbConnectionConfig)  -> Result<()> {
            Command::new(&self.0)
                .arg(format!("--dbname={}", dbcc.database_name()))
                .arg(format!("--host={}", dbcc.database_host()))
                .arg(format!("--port={}", dbcc.database_port()))
                .arg(format!("--username={}", dbcc.database_user()))
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .output()
                .map_err(Error::from)
                .and_then(|out| {
                    if out.status.success() {
                        info!("pgcli exited successfully");
                        Ok(())
                    } else {
                        Err(anyhow!("gpcli did not exit successfully"))
                            .with_context(|| {
                                match String::from_utf8(out.stderr) {
                                    Ok(log) => anyhow!("{}", log),
                                    Err(e)  => anyhow!("Cannot parse log into valid UTF-8: {}", e),
                                }
                            })
                            .map_err(Error::from)
                    }
                })
        }
    }

    struct PgCli(PathBuf);
    impl PgCliCommand for PgCli {
        fn run_for_uri(&self, dbcc: DbConnectionConfig)  -> Result<()> {
            Command::new(&self.0)
                .arg("--host")
                .arg(dbcc.database_host())
                .arg("--port")
                .arg(dbcc.database_port())
                .arg("--username")
                .arg(dbcc.database_user())
                .arg(dbcc.database_name())
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .output()
                .map_err(Error::from)
                .and_then(|out| {
                    if out.status.success() {
                        info!("pgcli exited successfully");
                        Ok(())
                    } else {
                        Err(anyhow!("gpcli did not exit successfully"))
                            .with_context(|| {
                                match String::from_utf8(out.stderr) {
                                    Ok(log) => anyhow!("{}", log),
                                    Err(e)  => anyhow!("Cannot parse log into valid UTF-8: {}", e),
                                }
                            })
                            .map_err(Error::from)
                    }
                })

        }
    }


    matches.value_of("tool")
        .map(|s| vec![s])
        .unwrap_or_else(|| vec!["psql", "pgcli"])
        .into_iter()
        .filter_map(|s| which::which(&s).ok().map(|path| (path, s)))
        .map(|(path, s)| {
            match s {
                "psql"  => Ok(Box::new(Psql(path)) as Box<dyn PgCliCommand>),
                "pgcli" => Ok(Box::new(PgCli(path)) as Box<dyn PgCliCommand>),
                prog    => Err(anyhow!("Unsupported pg CLI program: {}", prog)),
            }
        })
        .next()
        .transpose()?
        .ok_or_else(|| anyhow!("No Program found"))?
        .run_for_uri(db_connection_config)
}

fn artifacts(conn_cfg: DbConnectionConfig, matches: &ArgMatches) -> Result<()> {
    use crate::schema::artifacts::dsl;

    let csv  = matches.is_present("csv");
    let hdrs = mk_header(vec!["id", "path"]);
    let conn = crate::db::establish_connection(conn_cfg)?;
    let data = dsl::artifacts
        .load::<models::Artifact>(&conn)?
        .into_iter()
        .map(|artifact| vec![format!("{}", artifact.id), artifact.path])
        .collect::<Vec<_>>();

    if data.is_empty() {
        info!("No artifacts in database");
    } else {
        display_data(hdrs, data, csv)?;
    }

    Ok(())
}

fn envvars(conn_cfg: DbConnectionConfig, matches: &ArgMatches) -> Result<()> {
    use crate::schema::envvars::dsl;

    let csv  = matches.is_present("csv");
    let hdrs = mk_header(vec!["id", "name", "value"]);
    let conn = crate::db::establish_connection(conn_cfg)?;
    let data = dsl::envvars
        .load::<models::EnvVar>(&conn)?
        .into_iter()
        .map(|evar| {
            vec![format!("{}", evar.id), evar.name, evar.value]
        })
        .collect::<Vec<_>>();

    if data.is_empty() {
        info!("No environment variables in database");
    } else {
        display_data(hdrs, data, csv)?;
    }

    Ok(())
}

fn images(conn_cfg: DbConnectionConfig, matches: &ArgMatches) -> Result<()> {
    use crate::schema::images::dsl;

    let csv  = matches.is_present("csv");
    let hdrs = mk_header(vec!["id", "name"]);
    let conn = crate::db::establish_connection(conn_cfg)?;
    let data = dsl::images
        .load::<models::Image>(&conn)?
        .into_iter()
        .map(|image| {
            vec![format!("{}", image.id), image.name]
        })
        .collect::<Vec<_>>();

    if data.is_empty() {
        info!("No images in database");
    } else {
        display_data(hdrs, data, csv)?;
    }

    Ok(())
}

fn submits(conn_cfg: DbConnectionConfig, matches: &ArgMatches) -> Result<()> {
    use crate::schema::submits::dsl;

    let csv  = matches.is_present("csv");
    let hdrs = mk_header(vec!["id", "time", "uuid"]);
    let conn = crate::db::establish_connection(conn_cfg)?;
    let data = dsl::submits
        .load::<models::Submit>(&conn)?
        .into_iter()
        .map(|submit| {
            vec![format!("{}", submit.id), submit.submit_time.to_string(), submit.uuid.to_string()]
        })
        .collect::<Vec<_>>();

    if data.is_empty() {
        info!("No submits in database");
    } else {
        display_data(hdrs, data, csv)?;
    }

    Ok(())
}

fn jobs(conn_cfg: DbConnectionConfig, matches: &ArgMatches) -> Result<()> {
    use crate::schema::jobs::dsl;

    let csv  = matches.is_present("csv");
    let hdrs = mk_header(vec!["id", "submit uuid", "job uuid", "time", "endpoint", "success", "package", "version"]);
    let conn = crate::db::establish_connection(conn_cfg)?;
    let jobs = matches.value_of("submit_uuid")
        .map(uuid::Uuid::parse_str)
        .transpose()?
        .map(|submit_uuid| {
            use crate::schema;

            schema::jobs::table.inner_join({
                schema::submits::table.on(schema::submits::uuid.eq(submit_uuid))
            })
            .inner_join(schema::endpoints::table)
            .inner_join(schema::packages::table)
            .load::<(models::Job, models::Submit, models::Endpoint, models::Package)>(&conn)
        })
        .unwrap_or_else(|| {
            dsl::jobs
                .inner_join(crate::schema::submits::table)
                .inner_join(crate::schema::endpoints::table)
                .inner_join(crate::schema::packages::table)
                .load::<(models::Job, models::Submit, models::Endpoint, models::Package)>(&conn)
        })?;

    let data = jobs.into_iter()
        .map(|(job, submit, ep, package)| {
            let success = crate::log::ParsedLog::build_from(&job.log_text)?
                .is_successfull()
                .map(|b| if b {
                    String::from("yes")
                } else {
                    String::from("no")
                })
                .unwrap_or_else(|| String::from("unknown"));

            Ok(vec![format!("{}", job.id), submit.uuid.to_string(), job.uuid.to_string(), submit.submit_time.to_string(), ep.name, success, package.name, package.version])
        })
        .collect::<Result<Vec<_>>>()?;

    if data.is_empty() {
        info!("No submits in database");
    } else {
        display_data(hdrs, data, csv)?;
    }

    Ok(())
}

fn job<'a>(conn_cfg: DbConnectionConfig, config: &Configuration<'a>, matches: &ArgMatches) -> Result<()> {
    use std::io::Write;
    use colored::Colorize;
    use crate::schema;
    use crate::schema::jobs::dsl;

    let highlighting_disabled = matches.is_present("script_disable_highlighting");
    let configured_theme      = config.script_highlight_theme();
    let show_log              = matches.is_present("show_log");
    let show_script           = matches.is_present("show_script");
    let csv                   = matches.is_present("csv");
    let conn                  = crate::db::establish_connection(conn_cfg)?;
    let job_uuid              = matches.value_of("job_uuid")
        .map(uuid::Uuid::parse_str)
        .transpose()?
        .unwrap();

    let data = schema::jobs::table
        .filter(schema::jobs::dsl::uuid.eq(job_uuid))
        .inner_join(schema::submits::table)
        .inner_join(schema::endpoints::table)
        .inner_join(schema::packages::table)
        .inner_join(schema::images::table)
        .first::<(models::Job, models::Submit, models::Endpoint, models::Package, models::Image)>(&conn)?;

    let parsed_log = crate::log::ParsedLog::build_from(&data.0.log_text)?;
    let success = parsed_log.is_successfull();
    let s = indoc::formatdoc!(r#"
            Job:        {job_uuid}
            Succeeded:  {succeeded}
            Package:    {package_name} {package_version}

            Ran on:     {endpoint_name}
            Image:      {image_name}
            Container:  {container_hash}

            Script:     {script_len} lines
            Log:        {log_len} lines

            ---

            {script_text}

            ---

            {log_text}
        "#,

        job_uuid = match success {
            Some(true) => data.0.uuid.to_string().green(),
            Some(false) => data.0.uuid.to_string().red(),
            None => data.0.uuid.to_string().cyan(),
        },

        succeeded = match success {
            Some(true) => String::from("yes").green(),
            Some(false) => String::from("no").red(),
            None => String::from("unknown").cyan(),
        },
        package_name    = data.3.name.cyan(),
        package_version = data.3.version.cyan(),
        endpoint_name   = data.2.name.cyan(),
        image_name      = data.4.name.cyan(),
        container_hash  = data.0.container_hash.cyan(),
        script_len      = format!("{:<4}", data.0.script_text.lines().count()).cyan(),
        log_len         = format!("{:<4}", data.0.log_text.lines().count()).cyan(),
        script_text     = if show_script {
            if let Some(configured_theme) = configured_theme {
                if highlighting_disabled {
                    data.0.script_text.clone()
                } else {
                    // Load these once at the start of your program
                    let ps = SyntaxSet::load_defaults_newlines();
                    let ts = ThemeSet::load_defaults();

                    let syntax = ps.find_syntax_by_first_line(&data.0.script_text).ok_or_else(|| anyhow!("Failed to load syntax for highlighting script"))?;

                    let theme = ts.themes.get(configured_theme)
                        .ok_or_else(|| anyhow!("Theme not available: {}", configured_theme))?;
                    let mut h = HighlightLines::new(syntax, &theme);
                    LinesWithEndings::from(&data.0.script_text)
                        .map(|line| {
                            let ranges: Vec<(Style, &str)> = h.highlight(line, &ps);
                            as_24_bit_terminal_escaped(&ranges[..], true)
                        })
                        .join("")
                }
            } else {
                data.0.script_text.clone()
            }
        } else {
            String::from("<script hidden>")
        },
        log_text        = if show_log {
            parsed_log.iter()
                .map(|line_item| match line_item {
                    LogItem::Line(s)         => Ok(String::from_utf8(s.to_vec())?.normal()),
                    LogItem::Progress(u)     => Ok(format!("#BUTIDO:PROGRESS:{}", u).bright_black()),
                    LogItem::CurrentPhase(p) => Ok(format!("#BUTIDO:PHASE:{}", p).bright_black()),
                    LogItem::State(Ok(s))    => Ok(format!("#BUTIDO:STATE:OK:{}", s).green()),
                    LogItem::State(Err(s))   => Ok(format!("#BUTIDO:STATE:ERR:{}", s).red()),
                })
                .collect::<Result<Vec<_>>>()?
                .into_iter() // ugly, but hey... not important right now.
                .join("\n")
        } else {
            String::from("<log hidden>")
        },
    );

    writeln!(&mut std::io::stdout(), "{}", s).map_err(Error::from)
}


fn mk_header(vec: Vec<&str>) -> Vec<ascii_table::Column> {
    vec.into_iter()
        .map(|name| {
            let mut column = ascii_table::Column::default();
            column.header  = name.into();
            column.align   = ascii_table::Align::Left;
            column
        })
        .collect()
}

/// Display the passed data as nice ascii table,
/// or, if stdout is a pipe, print it nicely parseable
fn display_data<D: Display>(headers: Vec<ascii_table::Column>, data: Vec<Vec<D>>, csv: bool) -> Result<()> {
    use std::io::Write;

    if csv {
         use csv::WriterBuilder;
         let mut wtr = WriterBuilder::new().from_writer(vec![]);
         for record in data.into_iter() {
             let r: Vec<String> = record.into_iter()
                 .map(|e| e.to_string())
                 .collect();

             wtr.write_record(&r)?;
         }

        let out = std::io::stdout();
        let mut lock = out.lock();

         wtr.into_inner()
             .map_err(Error::from)
             .and_then(|t| String::from_utf8(t).map_err(Error::from))
             .and_then(|text| writeln!(lock, "{}", text).map_err(Error::from))

    } else {
        if atty::is(atty::Stream::Stdout) {
            let mut ascii_table = ascii_table::AsciiTable::default();

            ascii_table.max_width = terminal_size::terminal_size()
                .map(|tpl| tpl.0.0 as usize) // an ugly interface indeed!
                .unwrap_or(80);

            headers.into_iter()
                .enumerate()
                .for_each(|(i, c)| {
                    ascii_table.columns.insert(i, c);
                });

            ascii_table.print(data);
            Ok(())
        } else {
            let out = std::io::stdout();
            let mut lock = out.lock();
            for list in data {
                writeln!(lock, "{}", list.iter().map(|d| d.to_string()).join(" "))?;
            }
            Ok(())
        }
    }
}

//! Pager-owned Cursor cloud jobs.
//!
//! A `RemoteJob` is one Cursor cloud run. The list is not the single id that used to live on the
//! Cursor session. `& <task>` adds a job. `/cloud <id>` focuses one, and the next `& <task>` follows
//! that job. `&`, `& status`, and `& cancel` always talk to the focused job. `/cloud` and the tasks
//! pane read this same list.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use super::cloud::{self, CloudAgent, CloudAuth, CloudRepo, LOGIN_REQUIRED};

/// One Cursor cloud run started from this pager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteJob {
    pub id: String,
    pub session_id: String,
    pub repo_url: Option<String>,
    pub branch: Option<String>,
    pub url: Option<String>,
    pub run_id: Option<String>,
    pub run_status: Option<String>,
    pub result: Option<String>,
    pub pr_url: Option<String>,
    pub prompt: String,
}

/// Row the tasks pane renders for one cloud job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloudTaskRow {
    pub id: String,
    pub label: String,
    pub running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JobOp {
    Send(String),
    Refresh,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloudArgs {
    List,
    Focus(String),
    Cancel(String),
}

const NO_CLOUD_JOB: &str = "No cloud agent on this session yet. Send & and a task to start one.";

/// Jobs for every session in this process. The cursor thread and `/cloud` share it.
pub(crate) fn process() -> Arc<Mutex<CloudJobList>> {
    static JOBS: OnceLock<Arc<Mutex<CloudJobList>>> = OnceLock::new();
    JOBS.get_or_init(|| Arc::new(Mutex::new(CloudJobList::default())))
        .clone()
}

pub(crate) fn task_rows(session_id: Option<&str>) -> Vec<CloudTaskRow> {
    let Some(session_id) = session_id else {
        return Vec::new();
    };
    process()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .rows_for(session_id)
}

/// Apply a stream `result` event onto the shared list. The focused job is unchanged.
pub(crate) fn note_stream_result(agent_id: &str, status: &str, text: &str, pr_url: Option<&str>) {
    let jobs = process();
    let mut jobs = jobs.lock().unwrap_or_else(|err| err.into_inner());
    let Some(job) = jobs.jobs.iter_mut().find(|job| job.id == agent_id) else {
        return;
    };
    job.run_status = Some(status.to_owned());
    if !text.is_empty() {
        job.result = Some(text.to_owned());
    }
    if let Some(url) = pr_url {
        job.pr_url = Some(url.to_owned());
    }
}

pub(crate) fn run_command(session_id: &str, args: &str) -> Result<String, String> {
    let parsed = parse_cloud_args(args)?;
    let cwd = std::env::current_dir().ok();
    let repo = cwd.as_deref().and_then(cloud::repo_from_cwd);
    let repo_url = repo.as_ref().map(|repo| repo.url.clone());
    let api =
        std::env::var("CURSOR_CLOUD_API").unwrap_or_else(|_| "https://api.cursor.com".to_owned());
    let auth = if matches!(parsed, CloudArgs::Cancel(_)) {
        cloud::resolve_cloud_auth()
    } else {
        Ok(CloudAuth::Bearer(String::new()))
    };
    process()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .command(session_id, parsed, repo_url.as_deref(), &api, auth)
}

pub(crate) fn parse_cloud_args(args: &str) -> Result<CloudArgs, String> {
    let mut parts = args.split_whitespace();
    let Some(first) = parts.next() else {
        return Ok(CloudArgs::List);
    };
    if first.eq_ignore_ascii_case("cancel") {
        let Some(id) = parts.next() else {
            return Err("Usage: /cloud cancel <id>".to_owned());
        };
        if parts.next().is_some() {
            return Err("Usage: /cloud cancel <id>".to_owned());
        }
        return Ok(CloudArgs::Cancel(id.to_owned()));
    }
    if parts.next().is_some() {
        return Err("Usage: /cloud [id | cancel <id>]".to_owned());
    }
    Ok(CloudArgs::Focus(first.to_owned()))
}

pub(crate) fn classify_ampersand(raw: &str) -> Result<JobOp, String> {
    let Some(rest) = raw.trim().strip_prefix('&') else {
        return Err("not a cloud handoff".to_owned());
    };
    let rest = rest.trim();
    if rest.is_empty() || rest.eq_ignore_ascii_case("status") {
        Ok(JobOp::Refresh)
    } else if rest.eq_ignore_ascii_case("cancel") {
        Ok(JobOp::Cancel)
    } else {
        Ok(JobOp::Send(rest.to_owned()))
    }
}

#[derive(Debug, Default)]
pub(crate) struct CloudJobList {
    jobs: Vec<RemoteJob>,
    focused: HashMap<String, String>,
    /// Set by `/cloud <id>`. The next `& <task>` follows that job instead of creating one.
    follow: HashMap<String, bool>,
}

impl CloudJobList {
    /// Choose the cloud agent id for an `&` request.
    ///
    /// `Ok(None)` creates a job. `Ok(Some)` refreshes, cancels, or follows the focused job.
    /// A send follows only after `/cloud <id>`. Creating does not arm a follow-up, so the next
    /// `& <task>` starts another job.
    pub(crate) fn prepare(
        &mut self,
        session_id: &str,
        op: &JobOp,
    ) -> Result<Option<String>, String> {
        match op {
            JobOp::Refresh | JobOp::Cancel => self
                .focused
                .get(session_id)
                .cloned()
                .map(Some)
                .ok_or_else(|| NO_CLOUD_JOB.to_owned()),
            JobOp::Send(_) => {
                if self.follow.get(session_id).copied().unwrap_or(false) {
                    let id = self
                        .focused
                        .get(session_id)
                        .cloned()
                        .ok_or_else(|| NO_CLOUD_JOB.to_owned())?;
                    self.follow.insert(session_id.to_owned(), false);
                    Ok(Some(id))
                } else {
                    Ok(None)
                }
            }
        }
    }

    pub(crate) fn note_id(&mut self, session_id: &str, agent_id: &str) {
        if !self
            .jobs
            .iter()
            .any(|job| job.session_id == session_id && job.id == agent_id)
        {
            self.jobs.push(RemoteJob {
                id: agent_id.to_owned(),
                session_id: session_id.to_owned(),
                repo_url: None,
                branch: None,
                url: None,
                run_id: None,
                run_status: None,
                result: None,
                pr_url: None,
                prompt: String::new(),
            });
        }
        self.focused
            .insert(session_id.to_owned(), agent_id.to_owned());
        self.follow.insert(session_id.to_owned(), false);
    }

    pub(crate) fn commit(
        &mut self,
        session_id: &str,
        prompt: &str,
        repo: Option<&CloudRepo>,
        agent: &CloudAgent,
    ) -> RemoteJob {
        if let Some(job) = self
            .jobs
            .iter_mut()
            .find(|job| job.session_id == session_id && job.id == agent.id)
        {
            job.run_id = agent.run_id.clone().or(job.run_id.clone());
            job.run_status = agent.run_status.clone().or(job.run_status.clone());
            if agent.result.is_some() {
                job.result = agent.result.clone();
            }
            if agent.url.is_some() {
                job.url = agent.url.clone();
            }
            if agent.branch.is_some() {
                job.branch = agent.branch.clone();
            }
            if agent.pr_url.is_some() {
                job.pr_url = agent.pr_url.clone();
            }
            if job.repo_url.is_none()
                && let Some(repo) = repo
            {
                job.repo_url = Some(repo.url.clone());
                if job.branch.is_none() {
                    job.branch = repo.starting_ref.clone();
                }
            }
            if !prompt.is_empty() {
                job.prompt = prompt.to_owned();
            }
        } else {
            self.jobs.push(RemoteJob {
                id: agent.id.clone(),
                session_id: session_id.to_owned(),
                repo_url: repo.map(|repo| repo.url.clone()),
                branch: agent
                    .branch
                    .clone()
                    .or_else(|| repo.and_then(|repo| repo.starting_ref.clone())),
                url: agent.url.clone(),
                run_id: agent.run_id.clone(),
                run_status: agent.run_status.clone(),
                result: agent.result.clone(),
                pr_url: agent.pr_url.clone(),
                prompt: prompt.to_owned(),
            });
        }
        self.focused.insert(session_id.to_owned(), agent.id.clone());
        self.jobs
            .iter()
            .find(|job| job.session_id == session_id && job.id == agent.id)
            .cloned()
            .unwrap_or_else(|| RemoteJob {
                id: agent.id.clone(),
                session_id: session_id.to_owned(),
                repo_url: None,
                branch: None,
                url: None,
                run_id: None,
                run_status: None,
                result: None,
                pr_url: None,
                prompt: prompt.to_owned(),
            })
    }

    /// `&` against this list. Auth is the same `agent login` order as the HTTP client.
    /// Missing Cursor auth returns [`LOGIN_REQUIRED`] and does not open a connection.
    pub(crate) fn ampersand(
        &mut self,
        api_base: &str,
        auth_token: Option<&str>,
        api_key: Option<&str>,
        stored: Option<&cloud::StoredLogin>,
        session_id: &str,
        text: &str,
        repo: Option<&CloudRepo>,
        model_id: Option<&str>,
    ) -> Result<RemoteJob, String> {
        let auth = cloud::cloud_auth_from_sources(auth_token, api_key, stored)?;
        let op = classify_ampersand(text)?;
        let prompt = match &op {
            JobOp::Send(task) => task.clone(),
            JobOp::Refresh | JobOp::Cancel => String::new(),
        };
        let agent_id = self.prepare(session_id, &op)?;
        let repo_for_create = if agent_id.is_none() { repo } else { None };
        let agent = match &op {
            JobOp::Send(task) => cloud::dispatch(
                api_base,
                &auth,
                agent_id.as_deref(),
                task,
                repo_for_create,
                model_id,
            )?,
            JobOp::Refresh => {
                let id = agent_id.ok_or_else(|| NO_CLOUD_JOB.to_owned())?;
                cloud::refresh_existing(api_base, &auth, &id)?
            }
            JobOp::Cancel => {
                let id = agent_id.ok_or_else(|| NO_CLOUD_JOB.to_owned())?;
                cloud::cancel_existing(api_base, &auth, &id)?
            }
        };
        Ok(self.commit(session_id, &prompt, repo, &agent))
    }

    pub(crate) fn command(
        &mut self,
        session_id: &str,
        args: CloudArgs,
        repo_url: Option<&str>,
        api_base: &str,
        auth: Result<CloudAuth, String>,
    ) -> Result<String, String> {
        match args {
            CloudArgs::List => Ok(self.format_list(session_id, repo_url)),
            CloudArgs::Focus(id) => {
                let job = self.focus(session_id, &id)?;
                Ok(format_job(&job))
            }
            CloudArgs::Cancel(id) => {
                let auth = auth?;
                let job_id = self.resolve(session_id, &id)?.id.clone();
                let agent = cloud::cancel_existing(api_base, &auth, &job_id)?;
                let job = self.commit(session_id, "", None, &agent);
                Ok(format!("Cancelled {}.\n{}", job.id, format_job(&job)))
            }
        }
    }

    pub(crate) fn focus(&mut self, session_id: &str, id: &str) -> Result<RemoteJob, String> {
        let job = self.resolve(session_id, id)?.clone();
        self.focused.insert(session_id.to_owned(), job.id.clone());
        self.follow.insert(session_id.to_owned(), true);
        Ok(job)
    }

    pub(crate) fn list(&self, session_id: &str, repo_url: Option<&str>) -> Vec<RemoteJob> {
        self.jobs
            .iter()
            .filter(|job| visible(job, session_id, repo_url))
            .cloned()
            .collect()
    }

    pub(crate) fn rows_for(&self, session_id: &str) -> Vec<CloudTaskRow> {
        self.list(session_id, None)
            .into_iter()
            .map(|job| {
                let running = !cloud::run_is_terminal(job.run_status.as_deref());
                let focused = self.focused.get(session_id).is_some_and(|id| id == &job.id);
                CloudTaskRow {
                    id: job.id.clone(),
                    label: list_line(&job, focused),
                    running,
                }
            })
            .collect()
    }

    pub(crate) fn format_list(&self, session_id: &str, repo_url: Option<&str>) -> String {
        let jobs = self.list(session_id, repo_url);
        if jobs.is_empty() {
            return match repo_url {
                Some(url) => format!("No cloud jobs for {url}."),
                None => "No cloud jobs on this session.".to_owned(),
            };
        }
        let header = match repo_url {
            Some(url) => format!("Cloud jobs for {url} ({}):", jobs.len()),
            None => format!("Cloud jobs ({}):", jobs.len()),
        };
        let mut lines = Vec::with_capacity(jobs.len() + 1);
        lines.push(header);
        for job in &jobs {
            let focused = self.focused.get(session_id).is_some_and(|id| id == &job.id);
            lines.push(list_line(job, focused));
        }
        lines.join("\n")
    }

    fn resolve(&self, session_id: &str, id: &str) -> Result<&RemoteJob, String> {
        let mut exact = None;
        let mut prefixed = None;
        let mut prefix_count = 0usize;
        for job in self.jobs.iter().filter(|job| job.session_id == session_id) {
            if job.id == id {
                exact = Some(job);
                break;
            }
            if job.id.starts_with(id) {
                prefix_count += 1;
                prefixed = Some(job);
            }
        }
        if let Some(job) = exact {
            return Ok(job);
        }
        if prefix_count == 1
            && let Some(job) = prefixed
        {
            return Ok(job);
        }
        if prefix_count == 0 {
            Err(format!("No cloud job {id}."))
        } else {
            Err(format!("{id} matches more than one cloud job."))
        }
    }
}

fn visible(job: &RemoteJob, session_id: &str, repo_url: Option<&str>) -> bool {
    if job.session_id != session_id {
        return false;
    }
    match repo_url {
        None => true,
        Some(url) => match &job.repo_url {
            None => true,
            Some(job_url) => job_url == url,
        },
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn list_line(job: &RemoteJob, focused: bool) -> String {
    let mark = if focused { '*' } else { ' ' };
    let status = job.run_status.as_deref().unwrap_or("UNKNOWN");
    let branch = job.branch.as_deref().unwrap_or("-");
    let summary = job
        .result
        .as_deref()
        .filter(|text| !text.is_empty())
        .unwrap_or(job.prompt.as_str());
    format!(
        "{mark} {}  {status}  {branch}  {}",
        job.id,
        one_line(summary)
    )
}

fn format_job(job: &RemoteJob) -> String {
    let status = job.run_status.as_deref().unwrap_or("UNKNOWN");
    let branch = job.branch.as_deref().unwrap_or("unknown");
    let mut text = format!("{}\nRun status {status}.\nbranch {branch}\n", job.id);
    if let Some(result) = job.result.as_deref().filter(|text| !text.is_empty()) {
        text.push_str(result);
        if !result.ends_with('\n') {
            text.push('\n');
        }
    }
    if let Some(url) = &job.pr_url {
        text.push_str(url);
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    struct JoinOnDrop {
        stop: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl Drop for JoinOnDrop {
        fn drop(&mut self) {
            self.stop.store(false, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    struct Fake {
        next: u32,
        agents: HashMap<String, FakeAgent>,
        log: Vec<String>,
        bodies: Vec<String>,
    }

    struct FakeAgent {
        url: String,
        run_id: String,
        status: String,
        result: Option<String>,
        branch: Option<String>,
        pr_url: Option<String>,
    }

    fn reply(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn request_line(raw: &str) -> Option<(&str, &str)> {
        let line = raw.lines().next()?;
        let mut parts = line.split_whitespace();
        Some((parts.next()?, parts.next()?))
    }

    fn run_body(agent_id: &str, agent: &FakeAgent) -> String {
        let result = match &agent.result {
            Some(text) => format!(",\"result\":{text:?}"),
            None => String::new(),
        };
        let git = match (&agent.branch, &agent.pr_url) {
            (None, None) => String::new(),
            (branch, pr) => {
                let name = branch.as_deref().unwrap_or("");
                let pr_field = match pr {
                    Some(url) => format!(",\"prUrl\":{url:?}"),
                    None => String::new(),
                };
                format!(",\"git\":{{\"branches\":[{{\"name\":{name:?}{pr_field}}}]}}")
            }
        };
        format!(
            "{{\"id\":\"{}\",\"agentId\":\"{agent_id}\",\"status\":\"{}\"{result}{git}}}",
            agent.run_id, agent.status
        )
    }

    fn handle(req: &str, state: &Mutex<Fake>) -> String {
        let mut fake = state.lock().unwrap_or_else(|err| err.into_inner());
        let Some((method, path)) = request_line(req) else {
            return reply("400 Bad Request", "bad request");
        };
        fake.log.push(format!("{method} {path}"));
        let body = req
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("");
        if method == "POST" {
            fake.bodies.push(body.to_owned());
        }
        let parts: Vec<&str> = path
            .trim_start_matches('/')
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        if parts.first().copied() != Some("v1") || parts.get(1).copied() != Some("agents") {
            return reply("404 Not Found", "missing");
        }
        if parts.len() == 2 && method == "POST" {
            fake.next += 1;
            let n = fake.next;
            let id = format!("bc-{n}");
            let running = body.contains("write the docs");
            let agent = if running {
                FakeAgent {
                    url: format!("https://cursor.com/agents/{id}"),
                    run_id: format!("run-{n}"),
                    status: "RUNNING".to_owned(),
                    result: None,
                    branch: Some("feature".to_owned()),
                    pr_url: None,
                }
            } else {
                FakeAgent {
                    url: format!("https://cursor.com/agents/{id}"),
                    run_id: format!("run-{n}"),
                    status: "FINISHED".to_owned(),
                    result: Some("Refactored auth.".to_owned()),
                    branch: Some("feature".to_owned()),
                    pr_url: Some("https://github.com/acme/app/pull/4".to_owned()),
                }
            };
            let created = format!(
                "{{\"agent\":{{\"id\":\"{id}\",\"url\":\"{}\"}},\"run\":{{\"id\":\"{}\",\"status\":\"CREATING\"}}}}",
                agent.url, agent.run_id
            );
            fake.agents.insert(id, agent);
            return reply("200 OK", &created);
        }
        let Some(id) = parts.get(2).copied() else {
            return reply("404 Not Found", "missing id");
        };
        if !fake.agents.contains_key(id) {
            return reply("404 Not Found", "missing agent");
        }
        if parts.len() == 3 && method == "GET" {
            let agent = fake.agents.get(id).expect("checked");
            let body = format!(
                "{{\"id\":\"{id}\",\"url\":\"{}\",\"latestRunId\":\"{}\"}}",
                agent.url, agent.run_id
            );
            return reply("200 OK", &body);
        }
        if parts.len() == 4 && method == "POST" && parts.get(3).copied() == Some("runs") {
            fake.next += 1;
            let run_id = format!("run-{}", fake.next);
            let agent = fake.agents.get_mut(id).expect("checked");
            agent.run_id = run_id;
            if body.contains("add tests") {
                agent.status = "FINISHED".to_owned();
                agent.result = Some("Added the tests.".to_owned());
            }
            let body = format!(
                "{{\"run\":{{\"id\":\"{}\",\"agentId\":\"{id}\",\"status\":\"CREATING\"}}}}",
                agent.run_id
            );
            return reply("200 OK", &body);
        }
        if parts.len() == 5 && method == "GET" && parts.get(3).copied() == Some("runs") {
            let agent = fake.agents.get(id).expect("checked");
            return reply("200 OK", &run_body(id, agent));
        }
        if parts.len() == 6 && method == "POST" && parts.get(5).copied() == Some("cancel") {
            let agent = fake.agents.get_mut(id).expect("checked");
            agent.status = "CANCELLED".to_owned();
            agent.result = Some("Stopped.".to_owned());
            return reply("200 OK", &format!("{{\"id\":\"{}\"}}", agent.run_id));
        }
        reply("404 Not Found", "missing route")
    }

    fn start_fake() -> (String, Arc<Mutex<Fake>>, JoinOnDrop) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(Mutex::new(Fake {
            next: 0,
            agents: HashMap::new(),
            log: Vec::new(),
            bodies: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(true));
        let stop_thread = stop.clone();
        let state_thread = state.clone();
        let handle = thread::spawn(move || {
            while stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        sock.set_nonblocking(false).unwrap();
                        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                        let mut buf = [0_u8; 8192];
                        let Ok(n) = sock.read(&mut buf) else {
                            continue;
                        };
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let response = handle(&req, &state_thread);
                        let _ = sock.write_all(response.as_bytes());
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        (
            format!("http://{addr}"),
            state,
            JoinOnDrop {
                stop,
                handle: Some(handle),
            },
        )
    }

    fn repo() -> CloudRepo {
        CloudRepo {
            url: "https://github.com/acme/app".to_owned(),
            starting_ref: Some("feature".to_owned()),
        }
    }

    fn send(
        list: &mut CloudJobList,
        api: &str,
        session: &str,
        text: &str,
        repo: Option<&CloudRepo>,
    ) -> RemoteJob {
        list.ampersand(api, None, Some("test-key"), None, session, text, repo, None)
            .unwrap()
    }

    #[test]
    fn ampersand_without_cursor_auth_keeps_the_login_message() {
        let mut list = CloudJobList::default();
        let err = list
            .ampersand(
                "http://127.0.0.1:1",
                None,
                None,
                None,
                "s1",
                "& refactor the auth module",
                None,
                None,
            )
            .unwrap_err();
        assert_eq!(err, LOGIN_REQUIRED);
        assert!(err.contains("agent login"), "{err}");
        assert!(list.jobs.is_empty());
    }

    #[test]
    fn two_jobs_on_one_session_create_list_focus_follow_up_and_cancel() {
        let (api, state, _server) = start_fake();
        let mut list = CloudJobList::default();
        let repo = repo();
        let first = send(
            &mut list,
            &api,
            "s1",
            "& refactor the auth module",
            Some(&repo),
        );
        assert_eq!(first.id, "bc-1");
        assert_eq!(first.run_status.as_deref(), Some("FINISHED"));
        assert_eq!(first.result.as_deref(), Some("Refactored auth."));
        assert_eq!(first.branch.as_deref(), Some("feature"));
        assert_eq!(
            first.pr_url.as_deref(),
            Some("https://github.com/acme/app/pull/4")
        );
        let second = send(&mut list, &api, "s1", "& write the docs", Some(&repo));
        assert_eq!(second.id, "bc-2");
        assert_eq!(second.run_status.as_deref(), Some("RUNNING"));
        list.commit(
            "s1",
            "elsewhere",
            Some(&CloudRepo {
                url: "https://github.com/other/repo".to_owned(),
                starting_ref: Some("main".to_owned()),
            }),
            &CloudAgent {
                id: "bc-other".to_owned(),
                url: None,
                run_id: None,
                run_status: Some("FINISHED".to_owned()),
                result: Some("not this repo".to_owned()),
                branch: Some("main".to_owned()),
                pr_url: None,
            },
        );

        let listed = list
            .command(
                "s1",
                CloudArgs::List,
                Some(repo.url.as_str()),
                &api,
                Err("unused".to_owned()),
            )
            .unwrap();
        assert!(listed.contains("bc-1"), "{listed}");
        assert!(listed.contains("bc-2"), "{listed}");
        assert!(listed.contains("https://github.com/acme/app"), "{listed}");
        assert!(!listed.contains("bc-other"), "{listed}");
        assert!(!listed.contains("not this repo"), "{listed}");
        assert_eq!(list.list("s1", Some(repo.url.as_str())).len(), 2);

        let focused = list
            .command(
                "s1",
                CloudArgs::Focus("bc-1".to_owned()),
                Some(repo.url.as_str()),
                &api,
                Err("unused".to_owned()),
            )
            .unwrap();
        assert!(focused.contains("bc-1"), "{focused}");
        assert!(focused.contains("Refactored auth."), "{focused}");
        assert!(focused.contains("branch feature"), "{focused}");
        assert!(
            focused.contains("https://github.com/acme/app/pull/4"),
            "{focused}"
        );

        let followed = send(&mut list, &api, "s1", "& add tests", Some(&repo));
        assert_eq!(followed.id, "bc-1");
        assert_eq!(followed.result.as_deref(), Some("Added the tests."));
        assert_eq!(followed.run_status.as_deref(), Some("FINISHED"));
        let still_running = list.list("s1", Some(repo.url.as_str()));
        let other = still_running.iter().find(|job| job.id == "bc-2").unwrap();
        assert_eq!(other.run_status.as_deref(), Some("RUNNING"));

        let cancelled = list
            .command(
                "s1",
                CloudArgs::Cancel("bc-2".to_owned()),
                Some(repo.url.as_str()),
                &api,
                Ok(CloudAuth::Basic("test-key".to_owned())),
            )
            .unwrap();
        assert!(cancelled.contains("Cancelled bc-2."), "{cancelled}");
        assert!(cancelled.contains("CANCELLED"), "{cancelled}");
        let after = list.list("s1", Some(repo.url.as_str()));
        assert_eq!(after.len(), 2);
        let kept = after.iter().find(|job| job.id == "bc-1").unwrap();
        assert_eq!(kept.run_status.as_deref(), Some("FINISHED"));
        assert_eq!(kept.result.as_deref(), Some("Added the tests."));
        let stopped = after.iter().find(|job| job.id == "bc-2").unwrap();
        assert_eq!(stopped.run_status.as_deref(), Some("CANCELLED"));

        let rows = list.rows_for("s1");
        assert!(rows.iter().any(|row| row.id == "bc-1"));
        assert!(rows.iter().any(|row| row.id == "bc-2" && !row.running));

        let fake = state.lock().unwrap_or_else(|err| err.into_inner());
        assert_eq!(
            fake.log
                .iter()
                .filter(|line| line.as_str() == "POST /v1/agents")
                .count(),
            2,
            "{:?}",
            fake.log
        );
        assert!(
            fake.log
                .iter()
                .any(|line| line == "POST /v1/agents/bc-1/runs"),
            "{:?}",
            fake.log
        );
        assert!(
            fake.log
                .iter()
                .any(|line| line.starts_with("POST /v1/agents/bc-2/runs/")
                    && line.ends_with("/cancel")),
            "{:?}",
            fake.log
        );
        assert!(
            !fake
                .log
                .iter()
                .any(|line| line.contains("bc-1") && line.contains("cancel")),
            "{:?}",
            fake.log
        );
        assert!(
            fake.bodies.iter().any(|body| {
                body.contains("https://github.com/acme/app")
                    && body.contains("refactor the auth module")
                    && body.contains("\"startingRef\":\"feature\"")
            }),
            "{:?}",
            fake.bodies
        );
        assert!(
            fake.bodies
                .iter()
                .any(|body| body.contains("add tests") && !body.contains("repos")),
            "{:?}",
            fake.bodies
        );
    }

    #[test]
    fn cloud_list_of_twenty_fixture_jobs_is_under_50ms() {
        let mut list = CloudJobList::default();
        let repo = repo();
        for n in 0..20 {
            list.commit(
                "s1",
                &format!("task {n}"),
                Some(&repo),
                &CloudAgent {
                    id: format!("bc-{n}"),
                    url: None,
                    run_id: Some(format!("run-{n}")),
                    run_status: Some("FINISHED".to_owned()),
                    result: Some(format!("result {n}")),
                    branch: Some("feature".to_owned()),
                    pr_url: Some(format!("https://github.com/acme/app/pull/{n}")),
                },
            );
        }
        let started = std::time::Instant::now();
        let text = list
            .command(
                "s1",
                CloudArgs::List,
                Some(repo.url.as_str()),
                "http://127.0.0.1:1",
                Err("no auth".to_owned()),
            )
            .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(
            text.lines().filter(|line| line.contains("bc-")).count(),
            20,
            "{text}"
        );
        assert!(
            elapsed < Duration::from_millis(50),
            "cloud list of 20 fixture jobs took {elapsed:?}"
        );
        eprintln!("cloud list 20 fixture jobs: {elapsed:?}");
    }
}

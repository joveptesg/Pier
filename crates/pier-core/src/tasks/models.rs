//! Row shapes for the `task_templates` and `task_runs` tables. Serialized
//! as the JSON payloads the Tasks API returns; helpers below also produce
//! the inserts/updates the executor uses.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// Terminal vs. transient statuses for `task_runs.status`.
///
/// `pending` is the brief window between the row being inserted and the
/// agent's `/shell` POST returning a run id. `running` is the steady
/// state while we poll. Everything else is terminal.
pub fn is_terminal(status: &str) -> bool {
    matches!(
        status,
        "success" | "failed" | "cancelled" | "timeout" | "unreachable"
    )
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TaskTemplate {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub command: String,
    pub default_timeout_sec: i64,
    pub default_env_json: String,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TaskRun {
    pub id: String,
    pub template_id: Option<String>,
    pub server_id: String,
    pub batch_id: Option<String>,
    pub command_snapshot: String,
    pub env_json: String,
    pub timeout_sec: i64,
    pub status: String,
    pub exit_code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
    pub agent_run_id: Option<String>,
    pub triggered_by: String,
    pub error_message: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

pub fn template_get(conn: &Connection, id: &str) -> Result<Option<TaskTemplate>> {
    Ok(conn
        .query_row(
            "SELECT id, name, description, command, default_timeout_sec, default_env_json,
                    created_by, created_at, updated_at
             FROM task_templates WHERE id = ?1",
            [id],
            row_to_template,
        )
        .optional()?)
}

pub fn template_list(conn: &Connection) -> Result<Vec<TaskTemplate>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, description, command, default_timeout_sec, default_env_json,
                created_by, created_at, updated_at
         FROM task_templates
         ORDER BY name ASC",
    )?;
    let rows: Vec<TaskTemplate> = stmt
        .query_map([], row_to_template)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

fn row_to_template(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskTemplate> {
    Ok(TaskTemplate {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        command: row.get(3)?,
        default_timeout_sec: row.get(4)?,
        default_env_json: row.get(5)?,
        created_by: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

pub fn run_get(conn: &Connection, id: &str) -> Result<Option<TaskRun>> {
    Ok(conn
        .query_row(
            "SELECT id, template_id, server_id, batch_id, command_snapshot, env_json,
                    timeout_sec, status, exit_code, stdout, stderr, agent_run_id,
                    triggered_by, error_message, started_at, finished_at
             FROM task_runs WHERE id = ?1",
            [id],
            row_to_run,
        )
        .optional()?)
}

pub fn run_list(
    conn: &Connection,
    server_id: Option<&str>,
    template_id: Option<&str>,
    status: Option<&str>,
    limit: i64,
) -> Result<Vec<TaskRun>> {
    let mut sql = String::from(
        "SELECT id, template_id, server_id, batch_id, command_snapshot, env_json,
                timeout_sec, status, exit_code, stdout, stderr, agent_run_id,
                triggered_by, error_message, started_at, finished_at
         FROM task_runs WHERE 1=1",
    );
    let mut params_owned: Vec<String> = Vec::new();
    if let Some(s) = server_id {
        sql.push_str(" AND server_id = ?");
        params_owned.push(s.to_string());
    }
    if let Some(t) = template_id {
        sql.push_str(" AND template_id = ?");
        params_owned.push(t.to_string());
    }
    if let Some(st) = status {
        sql.push_str(" AND status = ?");
        params_owned.push(st.to_string());
    }
    sql.push_str(" ORDER BY started_at DESC LIMIT ?");
    let limit_clamped = limit.clamp(1, 500);
    params_owned.push(limit_clamped.to_string());

    let param_refs: Vec<&dyn rusqlite::ToSql> = params_owned
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<TaskRun> = stmt
        .query_map(param_refs.as_slice(), row_to_run)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRun> {
    Ok(TaskRun {
        id: row.get(0)?,
        template_id: row.get(1)?,
        server_id: row.get(2)?,
        batch_id: row.get(3)?,
        command_snapshot: row.get(4)?,
        env_json: row.get(5)?,
        timeout_sec: row.get(6)?,
        status: row.get(7)?,
        exit_code: row.get(8)?,
        stdout: row.get(9)?,
        stderr: row.get(10)?,
        agent_run_id: row.get(11)?,
        triggered_by: row.get(12)?,
        error_message: row.get(13)?,
        started_at: row.get(14)?,
        finished_at: row.get(15)?,
    })
}

/// Insert a pending row. Returns the new id.
#[allow(clippy::too_many_arguments)]
pub fn run_insert_pending(
    conn: &Connection,
    server_id: &str,
    template_id: Option<&str>,
    batch_id: Option<&str>,
    command_snapshot: &str,
    env_json: &str,
    timeout_sec: i64,
    triggered_by: &str,
) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO task_runs
            (id, template_id, server_id, batch_id, command_snapshot, env_json,
             timeout_sec, status, triggered_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8)",
        params![
            id,
            template_id,
            server_id,
            batch_id,
            command_snapshot,
            env_json,
            timeout_sec,
            triggered_by,
        ],
    )?;
    Ok(id)
}

/// Apply a snapshot from the agent. `status` is only advanced if we're not
/// already in a terminal state — handlers (cancel) may have moved us
/// forward concurrently.
#[allow(clippy::too_many_arguments)]
pub fn run_update_snapshot(
    conn: &Connection,
    id: &str,
    agent_run_id: Option<&str>,
    new_status: &str,
    exit_code: Option<i64>,
    stdout: &str,
    stderr: &str,
    finished: bool,
    error_message: Option<&str>,
) -> Result<()> {
    if finished {
        conn.execute(
            "UPDATE task_runs
                SET agent_run_id = COALESCE(?1, agent_run_id),
                    status = ?2,
                    exit_code = ?3,
                    stdout = ?4,
                    stderr = ?5,
                    error_message = ?6,
                    finished_at = COALESCE(finished_at, datetime('now'))
              WHERE id = ?7",
            params![
                agent_run_id,
                new_status,
                exit_code,
                stdout,
                stderr,
                error_message,
                id,
            ],
        )?;
    } else {
        conn.execute(
            "UPDATE task_runs
                SET agent_run_id = COALESCE(?1, agent_run_id),
                    status = CASE WHEN status IN ('cancelled','timeout','failed','success','unreachable')
                                  THEN status ELSE ?2 END,
                    stdout = ?3,
                    stderr = ?4
              WHERE id = ?5",
            params![agent_run_id, new_status, stdout, stderr, id],
        )?;
    }
    Ok(())
}

pub fn run_mark_unreachable(conn: &Connection, id: &str, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE task_runs
            SET status = 'unreachable',
                error_message = ?1,
                finished_at = COALESCE(finished_at, datetime('now'))
          WHERE id = ?2 AND status NOT IN ('success','failed','cancelled','timeout','unreachable')",
        params![error, id],
    )?;
    Ok(())
}

pub fn run_mark_cancelled(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE task_runs
            SET status = 'cancelled',
                finished_at = COALESCE(finished_at, datetime('now'))
          WHERE id = ?1 AND status IN ('pending','running')",
        params![id],
    )?;
    Ok(())
}

/// List runs that the executor needs to resume on core boot.
pub fn list_active_runs(conn: &Connection) -> Result<Vec<TaskRun>> {
    let mut stmt = conn.prepare(
        "SELECT id, template_id, server_id, batch_id, command_snapshot, env_json,
                timeout_sec, status, exit_code, stdout, stderr, agent_run_id,
                triggered_by, error_message, started_at, finished_at
         FROM task_runs
         WHERE status IN ('pending','running')",
    )?;
    let rows: Vec<TaskRun> = stmt
        .query_map([], row_to_run)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

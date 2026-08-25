//! The node's own read-only SQL check, run before anything is spawned.
//!
//! consulta has its own guard and keeps it. This is the first of two
//! independent checks, and the point of having two is that they now straddle a
//! privilege boundary: this one runs in the node, consulta's runs in the
//! sidecar, and a bypass needs both to be wrong.
//!
//! It refuses rather than sanitises. A query it cannot prove read-only is
//! rejected, which occasionally costs a legitimate query a rewrite.

use sqlparser::{
    ast::{Query, SetExpr, Statement},
    dialect::GenericDialect,
    keywords::Keyword,
    tokenizer::{Token, Tokenizer},
};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum GuardError {
    #[error("only SELECT and WITH queries are allowed (got {0})")]
    NotAQuery(String),
    #[error("exactly one statement is allowed, found {0}")]
    NotOneStatement(usize),
    #[error("{0} is not allowed in a read-only query")]
    Forbidden(String),
    #[error("could not parse the query: {0}")]
    Unparseable(String),
}

/// Keywords that must not appear anywhere, even inside a statement that parses
/// as a query. `FOR UPDATE` takes locks; the DDL and DML words would only
/// appear here through a parse the dialect read differently than the backend.
const FORBIDDEN: &[Keyword] = &[
    Keyword::INSERT,
    Keyword::UPDATE,
    Keyword::DELETE,
    Keyword::MERGE,
    Keyword::TRUNCATE,
    Keyword::DROP,
    Keyword::ALTER,
    Keyword::CREATE,
    Keyword::REPLACE,
    Keyword::GRANT,
    Keyword::REVOKE,
    Keyword::COMMIT,
    Keyword::ROLLBACK,
    Keyword::SAVEPOINT,
    Keyword::LOCK,
    Keyword::CALL,
    Keyword::EXEC,
    Keyword::EXECUTE,
    Keyword::PRAGMA,
    Keyword::ATTACH,
    Keyword::DETACH,
    Keyword::VACUUM,
    Keyword::ANALYZE,
    Keyword::COPY,
    Keyword::SET,
];

pub fn assert_read_only(sql: &str) -> Result<(), GuardError> {
    let dialect = GenericDialect {};

    // Token pass first: it sees keywords the parser may fold into an
    // expression, and it sees them outside string literals.
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize()
        .map_err(|e| GuardError::Unparseable(e.to_string()))?;
    for token in &tokens {
        if let Token::Word(w) = token {
            // A quoted word is an identifier, not a keyword: a column named
            // "delete" is data, and refusing it would be the guard being wrong.
            if w.quote_style.is_none() && FORBIDDEN.contains(&w.keyword) {
                return Err(GuardError::Forbidden(format!("{:?}", w.keyword)));
            }
        }
    }

    let statements = sqlparser::parser::Parser::parse_sql(&dialect, sql)
        .map_err(|e| GuardError::Unparseable(e.to_string()))?;
    if statements.len() != 1 {
        return Err(GuardError::NotOneStatement(statements.len()));
    }
    match &statements[0] {
        Statement::Query(q) => match query_writes(q) {
            // A query that parses as a SELECT can still write or lock: `SELECT …
            // INTO t` creates and populates a table on MSSQL (where consulta's
            // own guard does not reject writes), and a locking clause takes row
            // locks. Both are refused on the parsed AST, which also means a
            // string literal containing "FOR UPDATE" no longer trips the guard.
            Some(what) => Err(GuardError::Forbidden(what.into())),
            None => Ok(()),
        },
        other => Err(GuardError::NotAQuery(statement_name(other))),
    }
}

/// The reason a query is not read-only, or `None` if it is. Walks CTEs, set
/// operations, and subqueries so a write nested inside is caught too.
fn query_writes(q: &Query) -> Option<&'static str> {
    if !q.locks.is_empty() {
        return Some("a locking clause (FOR UPDATE / FOR SHARE)");
    }
    if let Some(with) = &q.with {
        for cte in &with.cte_tables {
            if let Some(w) = query_writes(&cte.query) {
                return Some(w);
            }
        }
    }
    body_writes(&q.body)
}

fn body_writes(body: &SetExpr) -> Option<&'static str> {
    match body {
        SetExpr::Select(s) if s.into.is_some() => Some("SELECT … INTO"),
        SetExpr::Query(q) => query_writes(q),
        SetExpr::SetOperation { left, right, .. } => {
            body_writes(left).or_else(|| body_writes(right))
        }
        _ => None,
    }
}

fn statement_name(s: &Statement) -> String {
    let text = format!("{s}");
    text.split_whitespace()
        .next()
        .unwrap_or("statement")
        .to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_queries_pass() {
        for sql in [
            "SELECT 1",
            "select id, name from people where id = :id",
            "WITH recent AS (SELECT * FROM people) SELECT count(*) FROM recent",
            "SELECT a.id FROM people a JOIN roles b ON a.id = b.person_id ORDER BY a.id",
        ] {
            assert_eq!(assert_read_only(sql), Ok(()), "should allow: {sql}");
        }
    }

    #[test]
    fn writes_are_refused() {
        for sql in [
            "DELETE FROM people",
            "INSERT INTO people VALUES (1, 'x')",
            "UPDATE people SET name = 'x'",
            "DROP TABLE people",
            "TRUNCATE TABLE people",
            "CREATE TABLE t (id int)",
            "GRANT SELECT ON people TO other",
        ] {
            assert!(assert_read_only(sql).is_err(), "should refuse: {sql}");
        }
    }

    #[test]
    fn stacked_statements_are_refused() {
        // The classic bypass: a read followed by a write.
        assert!(assert_read_only("SELECT 1; DELETE FROM people").is_err());
    }

    #[test]
    fn locking_reads_are_refused() {
        for sql in [
            "SELECT * FROM people FOR UPDATE",
            "SELECT * FROM people FOR SHARE",
            "SELECT * FROM people FOR NO KEY UPDATE",
        ] {
            assert!(assert_read_only(sql).is_err(), "should refuse: {sql}");
        }
    }

    #[test]
    fn select_into_is_refused() {
        // Parses as a query, but on MSSQL `SELECT … INTO t` creates and
        // populates a table — a write the backend's own guard may not catch.
        for sql in [
            "SELECT * INTO backup FROM people",
            "SELECT id INTO #tmp FROM people",
            "WITH c AS (SELECT 1 AS x) SELECT x INTO keep FROM c",
        ] {
            assert!(assert_read_only(sql).is_err(), "should refuse: {sql}");
        }
    }

    #[test]
    fn for_update_inside_a_string_literal_is_allowed() {
        // The old reserialize-and-substring check refused this; the AST check
        // does not, because the words are data.
        assert_eq!(
            assert_read_only("SELECT * FROM notes WHERE body = 'please FOR UPDATE the row'"),
            Ok(())
        );
    }

    #[test]
    fn a_write_hidden_in_a_cte_is_refused() {
        assert!(
            assert_read_only("WITH x AS (DELETE FROM people RETURNING id) SELECT * FROM x")
                .is_err()
        );
    }

    #[test]
    fn nonsense_is_refused_rather_than_passed_through() {
        assert!(assert_read_only("not sql at all ((").is_err());
        assert!(assert_read_only("").is_err());
    }

    #[test]
    fn a_keyword_inside_a_string_literal_does_not_trip_the_guard() {
        // Refusing this would be the guard being wrong in the safe direction,
        // but it is still wrong: the word is data, not a statement.
        assert_eq!(
            assert_read_only("SELECT * FROM people WHERE name = 'delete me'"),
            Ok(())
        );
    }
}

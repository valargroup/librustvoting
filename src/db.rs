use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension as _};

pub fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS properties(
        id_property INTEGER PRIMARY KEY,
        name TEXT NOT NULL UNIQUE,
        value TEXT NOT NULL)",
        [],
    )?;
    connection.execute(
        "CREATE TABLE IF NOT EXISTS ballots(
        id_ballot INTEGER PRIMARY KEY,
        election INTEGER NOT NULL,
        height INTEGER NOT NULL,
        hash BLOB NOT NULL UNIQUE,
        data BLOB NOT NULL)",
        [],
    )?;
    connection.execute(
        "CREATE TABLE IF NOT EXISTS nfs(
        id_nf INTEGER PRIMARY KEY NOT NULL,
        election INTEGER NOT NULL,
        hash BLOB NOT NULL UNIQUE)",
        [],
    )?;
    connection.execute(
        "CREATE TABLE IF NOT EXISTS dnfs(
        id_dnf INTEGER PRIMARY KEY NOT NULL,
        election INTEGER NOT NULL,
        hash BLOB NOT NULL UNIQUE)",
        [],
    )?;
    Ok(())
}

pub fn store_prop(connection: &Connection, name: &str, value: &str) -> Result<()> {
    connection.execute(
        "INSERT INTO properties(name, value) VALUES (?1, ?2)
        ON CONFLICT (name) DO UPDATE SET value = excluded.value",
        params![name, value],
    )?;
    Ok(())
}

pub fn load_prop(connection: &Connection, name: &str) -> Result<Option<String>> {
    let value = connection
        .query_row(
            "SELECT value FROM properties WHERE name = ?1",
            [name],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    Ok(value)
}

pub fn store_dnf(connection: &Connection, id_election: u32, dnf: &[u8]) -> Result<()> {
    connection.execute(
        "INSERT INTO dnfs(election, hash) VALUES (?1, ?2)",
        params![id_election, dnf],
    )?;
    Ok(())
}


use std::time::Duration;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rusqlite::{Connection, params};
use shelves::{acl, schema, search, storage};

fn bench_conn(memory_count: usize) -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory bench db");
    schema::init_db(&conn).expect("init bench schema");
    for idx in 0..memory_count {
        conn.execute(
            "INSERT INTO memories(
                name, title, body, owner, scope, source_path, content_hash, is_lock,
                created_at, updated_at
             ) VALUES(?1, ?2, ?3, 'shared', 'company', ?4, ?5, ?6,
                '2026-06-01T00:00:00Z', '2026-06-01T00:00:00Z')",
            params![
                format!("bench-memory-{idx:05}"),
                format!("Bench Memory {idx:05}"),
                "barista memory recall benchmark body",
                format!("/tmp/bench-memory-{idx:05}.md"),
                format!("hash-{idx:05}"),
                i64::from(idx % 200 == 0),
            ],
        )
        .expect("insert bench memory");
        let memory_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO recall_events(memory_id, queried_by, query_scope, ts)
             VALUES(?1, 'bench', 'company', '2026-06-10T00:00:00Z')",
            params![memory_id],
        )
        .expect("insert bench recall event");
    }
    conn
}

fn acl_conn() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory ACL bench db");
    schema::init_db(&conn).expect("init ACL bench schema");
    conn.execute(
        "INSERT INTO node_acl(owner_node, reader, granted)
         VALUES('agent:archivist', 'agent:engineer', 1)",
        [],
    )
    .expect("insert explicit ACL");
    conn.execute(
        "INSERT INTO node_acl(owner_node, reader, granted)
         VALUES('agent:orchestrator', '*', 0)",
        [],
    )
    .expect("insert wildcard ACL");
    conn
}

fn activation_recompute(c: &mut Criterion) {
    let mut group = c.benchmark_group("activation_recompute_stats");
    for memory_count in [1_000usize, 10_000] {
        let conn = bench_conn(memory_count);
        group.bench_function(format!("{memory_count}_memories"), |b| {
            b.iter(|| {
                black_box(storage::stats(&conn, &[]).expect("compute stats"));
            });
        });
    }
    group.finish();
}

fn scope_fallthrough(c: &mut Criterion) {
    c.bench_function("scope_fallthrough_product", |b| {
        b.iter(|| black_box(search::scope_fallthrough(black_box("product:notebook"))));
    });
    c.bench_function("scope_fallthrough_company", |b| {
        b.iter(|| black_box(search::scope_fallthrough(black_box("company"))));
    });
    c.bench_function("scope_fallthrough_os", |b| {
        b.iter(|| black_box(search::scope_fallthrough(black_box("os"))));
    });
}

fn acl_resolution(c: &mut Criterion) {
    let conn = acl_conn();
    c.bench_function("acl_shared_owner", |b| {
        b.iter(|| {
            black_box(acl::can_read_owner(
                &conn,
                black_box("shared"),
                black_box("agent:engineer"),
            ))
            .expect("shared ACL")
        });
    });
    c.bench_function("acl_explicit_reader", |b| {
        b.iter(|| {
            black_box(acl::can_read_owner(
                &conn,
                black_box("agent:archivist"),
                black_box("agent:engineer"),
            ))
            .expect("explicit ACL")
        });
    });
    c.bench_function("acl_wildcard_reader", |b| {
        b.iter(|| {
            black_box(acl::can_read_owner(
                &conn,
                black_box("agent:orchestrator"),
                black_box("agent:engineer"),
            ))
            .expect("wildcard ACL")
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .frontendple_size(10)
        .warm_up_time(Duration::from_millis(200))
        .measurement_time(Duration::from_millis(500));
    targets = activation_recompute, scope_fallthrough, acl_resolution
}
criterion_main!(benches);

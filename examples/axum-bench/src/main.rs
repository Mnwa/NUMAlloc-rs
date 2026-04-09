#[cfg(feature = "numalloc")]
#[global_allocator]
static ALLOC: numalloc::NumaAlloc = numalloc::NumaAlloc::new();

#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::HashMap;

use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

#[derive(Serialize)]
struct Small {
    id: u64,
    active: bool,
}

#[derive(Serialize)]
struct Medium {
    id: u64,
    name: String,
    email: String,
    tags: Vec<String>,
    scores: Vec<f64>,
}

#[derive(Serialize)]
struct Large {
    id: u64,
    title: String,
    description: String,
    metadata: HashMap<String, String>,
    items: Vec<Medium>,
}

async fn small_handler() -> Json<Small> {
    Json(Small {
        id: 1,
        active: true,
    })
}

async fn medium_handler() -> Json<Medium> {
    Json(Medium {
        id: 42,
        name: "Alice Johnson".to_string(),
        email: "alice@example.com".to_string(),
        tags: vec![
            "rust".to_string(),
            "performance".to_string(),
            "numa".to_string(),
            "allocator".to_string(),
        ],
        scores: vec![95.5, 88.0, 92.3, 78.9, 99.1],
    })
}

async fn large_handler() -> Json<Large> {
    let mut metadata = HashMap::new();
    for i in 0..20 {
        metadata.insert(
            format!("key_{i}"),
            format!("value_{i}_with_some_extra_data"),
        );
    }

    let items: Vec<Medium> = (0..50)
        .map(|i| Medium {
            id: i,
            name: format!("User {i}"),
            email: format!("user{i}@example.com"),
            tags: vec![format!("tag_{}", i % 5), format!("group_{}", i % 3)],
            scores: vec![(i as f64) * 1.1, (i as f64) * 2.2, (i as f64) * 3.3],
        })
        .collect();

    Json(Large {
        id: 1,
        title: "Large dataset response".to_string(),
        description:
            "This response contains a large nested JSON structure to stress-test allocations"
                .to_string(),
        metadata,
        items,
    })
}

async fn bulk_handler() -> Json<Vec<Medium>> {
    let items: Vec<Medium> = (0..200)
        .map(|i| Medium {
            id: i,
            name: format!("Bulk user {i}"),
            email: format!("bulk{i}@example.com"),
            tags: vec![format!("category_{}", i % 10), format!("region_{}", i % 4)],
            scores: vec![(i as f64) * 0.5; 10],
        })
        .collect();

    Json(items)
}

#[tokio::main]
async fn main() {
    let allocator_name = if cfg!(feature = "numalloc") {
        "numalloc"
    } else if cfg!(feature = "mimalloc") {
        "mimalloc"
    } else {
        "system"
    };

    println!("Starting axum-bench with allocator: {allocator_name}");
    println!("PID: {}", std::process::id());
    println!("Endpoints:");
    println!("  GET /small  - ~32 bytes JSON");
    println!("  GET /medium - ~256 bytes JSON");
    println!("  GET /large  - ~16 KB JSON");
    println!("  GET /bulk   - ~64 KB JSON");

    let app = Router::new()
        .route("/small", get(small_handler))
        .route("/medium", get(medium_handler))
        .route("/large", get(large_handler))
        .route("/bulk", get(bulk_handler));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .expect("Failed to bind to port 3000");

    println!("Listening on http://127.0.0.1:3000");

    axum::serve(listener, app).await.expect("Server error");
}

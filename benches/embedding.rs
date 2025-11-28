//! Arrow batch embedding benchmarks
//!
//! Benchmarks for Arrow IPC batch processing at various sizes.
//! These benchmarks measure the pure serialization/deserialization overhead
//! without requiring a live TEI instance.

use arrow::array::{ArrayRef, FixedSizeListArray, Float32Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::io::Cursor;
use std::sync::Arc;

/// Create a test Arrow IPC batch with the given number of text items
fn create_test_batch(size: usize) -> Vec<u8> {
    let texts: Vec<&str> = (0..size)
        .map(|_| "The quick brown fox jumps over the lazy dog.")
        .collect();

    let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
    let array = StringArray::from(texts);
    let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(array) as ArrayRef]).unwrap();

    let mut buffer = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut buffer, &schema).unwrap();
        writer.write(&batch).unwrap();
        writer.finish().unwrap();
    }
    buffer
}

/// Deserialize Arrow IPC and extract text array
fn deserialize_batch(arrow_ipc: &[u8]) -> StringArray {
    let cursor = Cursor::new(arrow_ipc);
    let mut reader = StreamReader::try_new(cursor, None).unwrap();
    let batch = reader.next().unwrap().unwrap();

    batch
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap()
        .clone()
}

/// Create embedding result batch (simulates noop mode output)
fn create_embedding_result(num_rows: usize, embedding_dim: i32) -> Vec<u8> {
    let flat_embeddings = vec![0.0f32; num_rows * embedding_dim as usize];
    let values = Arc::new(Float32Array::from(flat_embeddings)) as ArrayRef;

    let field = Arc::new(Field::new("item", DataType::Float32, false));
    let embeddings_array = FixedSizeListArray::new(field, embedding_dim, values, None);

    let schema = Arc::new(Schema::new(vec![Field::new(
        "embeddings",
        DataType::FixedSizeList(
            Arc::new(Field::new("item", DataType::Float32, false)),
            embedding_dim,
        ),
        false,
    )]));

    let result_batch =
        RecordBatch::try_new(schema, vec![Arc::new(embeddings_array) as ArrayRef]).unwrap();

    let mut buffer = Vec::new();
    {
        use arrow::ipc::CompressionType;
        use arrow::ipc::writer::IpcWriteOptions;

        let write_options = IpcWriteOptions::default()
            .try_with_compression(Some(CompressionType::LZ4_FRAME))
            .unwrap();

        let mut writer =
            StreamWriter::try_new_with_options(&mut buffer, &result_batch.schema(), write_options)
                .unwrap();

        writer.write(&result_batch).unwrap();
        writer.finish().unwrap();
    }
    buffer
}

/// Full round-trip: deserialize input -> simulate processing -> serialize output
fn process_batch_roundtrip(arrow_ipc: &[u8], embedding_dim: i32) -> Vec<u8> {
    // Deserialize input
    let cursor = Cursor::new(arrow_ipc);
    let mut reader = StreamReader::try_new(cursor, None).unwrap();
    let batch = reader.next().unwrap().unwrap();
    let num_rows = batch.num_rows();

    // Create output (simulates noop embedding)
    create_embedding_result(num_rows, embedding_dim)
}

/// Benchmark Arrow batch serialization (input creation)
fn bench_arrow_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("arrow_serialize");

    for size in [100, 1000, 10000] {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("batch_size", size), &size, |b, &size| {
            b.iter(|| create_test_batch(black_box(size)));
        });
    }
    group.finish();
}

/// Benchmark Arrow batch deserialization
fn bench_arrow_deserialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("arrow_deserialize");

    for size in [100, 1000, 10000] {
        let batch = create_test_batch(size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("batch_size", size), &batch, |b, batch| {
            b.iter(|| deserialize_batch(black_box(batch)));
        });
    }
    group.finish();
}

/// Benchmark embedding result creation (output serialization)
fn bench_embedding_result_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("arrow_embed_result");
    let embedding_dim = 384i32; // Standard BGE-small

    for size in [100, 1000, 10000] {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("batch_size", size), &size, |b, &size| {
            b.iter(|| create_embedding_result(black_box(size), embedding_dim));
        });
    }
    group.finish();
}

/// Benchmark full roundtrip (deserialize -> process -> serialize)
fn bench_arrow_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("arrow_roundtrip");
    let embedding_dim = 384i32;

    for size in [100, 1000, 10000] {
        let batch = create_test_batch(size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("batch_size", size), &batch, |b, batch| {
            b.iter(|| process_batch_roundtrip(black_box(batch), embedding_dim));
        });
    }
    group.finish();
}

/// Benchmark showing scaling behavior across batch sizes
fn bench_arrow_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("arrow_scaling");
    let embedding_dim = 384i32;

    // Test scaling from small to large batches
    for size in [10, 50, 100, 500, 1000, 5000, 10000] {
        let batch = create_test_batch(size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("items", size), &batch, |b, batch| {
            b.iter(|| process_batch_roundtrip(black_box(batch), embedding_dim));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_arrow_serialize,
    bench_arrow_deserialize,
    bench_embedding_result_creation,
    bench_arrow_roundtrip,
    bench_arrow_scaling,
);
criterion_main!(benches);

// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! A generic stream over file format readers that can be used by
//! any file format that read its files from start to end.
//!
//! Note: Most traits here need to be marked `Sync + Send` to be
//! compliant with the `SendableRecordBatchStream` trait.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::file_format::{ObjectMeta, PartitionedFile};
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::{error::Result as ArrowResult, record_batch::RecordBatch};
use datafusion::common::ScalarValue;
use datafusion::datasource::listing::FileRange;
use datafusion::error::Result;
use datafusion::execution::context::TaskContext;
use datafusion::physical_plan::metrics::BaselineMetrics;
use datafusion::physical_plan::RecordBatchStream;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{ready, FutureExt, Stream, StreamExt};

use crate::file_format::{FileScanConfig, PartitionColumnProjector};
use crate::util::fs::FsProvider;

/// A fallible future that resolves to a stream of [`RecordBatch`]
pub type ReaderFuture =
    BoxFuture<'static, Result<BoxStream<'static, ArrowResult<RecordBatch>>>>;

pub trait FormatReader: Unpin {
    fn open(
        &self,
        fs_provider: Arc<FsProvider>,
        file: ObjectMeta,
        range: Option<FileRange>,
    ) -> ReaderFuture;
}

/// A stream that iterates record batch by record batch, file over file.
pub struct FileStream<F: FormatReader> {
    /// An iterator over input files.
    file_iter: VecDeque<PartitionedFile>,
    /// The stream schema (file schema including partition columns and after
    /// projection).
    projected_schema: SchemaRef,
    /// The remaining number of records to parse, None if no limit
    remain: Option<usize>,
    /// A closure that takes a reader and an optional remaining number of lines
    /// (before reaching the limit) and returns a batch iterator. If the file reader
    /// is not capable of limiting the number of records in the last batch, the file
    /// stream will take care of truncating it.
    file_reader: F,
    /// The partition column projector
    pc_projector: PartitionColumnProjector,
    /// the input fs
    fs_provider: Arc<FsProvider>,
    /// The stream state
    state: FileStreamState,
    /// Baseline metrics
    baseline_metrics: BaselineMetrics,
}

enum FileStreamState {
    /// The idle state, no file is currently being read
    Idle,
    /// Currently performing asynchronous IO to obtain a stream of RecordBatch
    /// for a given parquet file
    Open {
        /// A [`ReaderFuture`] returned by [`FormatReader::open`]
        future: ReaderFuture,
        /// The partition values for this file
        partition_values: Vec<ScalarValue>,
    },
    /// Scanning the [`BoxStream`] returned by the completion of a [`ReaderFuture`]
    /// returned by [`FormatReader::open`]
    Scan {
        /// Partitioning column values for the current batch_iter
        partition_values: Vec<ScalarValue>,
        /// The reader instance
        reader: BoxStream<'static, ArrowResult<RecordBatch>>,
    },
    /// Encountered an error
    Error,
    /// Reached the row limit
    Limit,
}

impl<F: FormatReader> FileStream<F> {
    pub fn new(
        fs_provider: Arc<FsProvider>,
        config: &FileScanConfig,
        partition: usize,
        _context: Arc<TaskContext>,
        file_reader: F,
        baseline_metrics: BaselineMetrics,
    ) -> Result<Self> {
        let (projected_schema, _) = config.project();
        let pc_projector = PartitionColumnProjector::new(
            projected_schema.clone(),
            &config.table_partition_cols,
        );

        let files = config.file_groups[partition].clone();

        Ok(Self {
            file_iter: files.into(),
            projected_schema,
            remain: config.limit,
            file_reader,
            pc_projector,
            fs_provider,
            state: FileStreamState::Idle,
            baseline_metrics,
        })
    }

    fn poll_inner(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<ArrowResult<RecordBatch>>> {
        loop {
            match &mut self.state {
                FileStreamState::Idle => {
                    let file = match self.file_iter.pop_front() {
                        Some(file) => file,
                        None => return Poll::Ready(None),
                    };

                    let future = self.file_reader.open(
                        self.fs_provider.clone(),
                        file.object_meta,
                        file.range,
                    );

                    self.state = FileStreamState::Open {
                        future,
                        partition_values: file.partition_values,
                    }
                }
                FileStreamState::Open {
                    future,
                    partition_values,
                } => match ready!(future.poll_unpin(cx)) {
                    Ok(reader) => {
                        self.state = FileStreamState::Scan {
                            partition_values: std::mem::take(partition_values),
                            reader,
                        };
                    }
                    Err(e) => {
                        self.state = FileStreamState::Error;
                        return Poll::Ready(Some(Err(e.into())));
                    }
                },
                FileStreamState::Scan {
                    reader,
                    partition_values,
                } => match ready!(reader.poll_next_unpin(cx)) {
                    Some(result) => {
                        let result = result
                            .and_then(|b| self.pc_projector.project(b, partition_values))
                            .map(|batch| match &mut self.remain {
                                Some(remain) => {
                                    if *remain > batch.num_rows() {
                                        *remain -= batch.num_rows();
                                        batch
                                    } else {
                                        let batch = batch.slice(0, *remain);
                                        self.state = FileStreamState::Limit;
                                        *remain = 0;
                                        batch
                                    }
                                }
                                None => batch,
                            });

                        if result.is_err() {
                            self.state = FileStreamState::Error
                        }

                        return Poll::Ready(Some(result));
                    }
                    None => self.state = FileStreamState::Idle,
                },
                FileStreamState::Error | FileStreamState::Limit => {
                    return Poll::Ready(None)
                }
            }
        }
    }
}

impl<F: FormatReader> Stream for FileStream<F> {
    type Item = ArrowResult<RecordBatch>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let result = self.poll_inner(cx);
        self.baseline_metrics.record_poll(result)
    }
}

impl<F: FormatReader> RecordBatchStream for FileStream<F> {
    fn schema(&self) -> SchemaRef {
        self.projected_schema.clone()
    }
}

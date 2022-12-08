/*
 * Copyright 2022 The Blaze Authors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package org.apache.spark.sql.execution.blaze.plan

import scala.concurrent.ExecutionContext.Implicits.global
import scala.concurrent.Future

import org.apache.spark._

import org.apache.spark.rdd.MapPartitionsRDD
import org.apache.spark.rdd.RDD
import org.apache.spark.scheduler.MapStatus
import org.apache.spark.shuffle.ShuffleWriteMetricsReporter
import org.apache.spark.shuffle.ShuffleWriteProcessor
import org.apache.spark.sql.blaze.NativeRDD
import org.apache.spark.sql.catalyst.InternalRow
import org.apache.spark.sql.catalyst.errors.attachTree
import org.apache.spark.sql.catalyst.plans.logical.Statistics
import org.apache.spark.sql.catalyst.plans.physical._
import org.apache.spark.sql.execution._
import org.apache.spark.sql.execution.metric.SQLMetric
import org.apache.spark.sql.execution.metric.SQLMetrics
import org.apache.spark.sql.execution.metric.SQLShuffleReadMetricsReporter
import org.apache.spark.sql.execution.metric.SQLShuffleWriteMetricsReporter

case class ArrowShuffleExchangeExec(
    override val outputPartitioning: Partitioning,
    override val child: SparkPlan,
    noUserSpecifiedNumPartition: Boolean = true)
    extends ArrowShuffleExchangeBase(outputPartitioning, child) {

  // NOTE: coordinator can be null after serialization/deserialization,
  //       e.g. it can be null on the Executor side
  lazy val writeMetrics: Map[String, SQLMetric] =
    SQLShuffleWriteMetricsReporter.createShuffleWriteMetrics(sparkContext)
  lazy val readMetrics: Map[String, SQLMetric] =
    SQLShuffleReadMetricsReporter.createShuffleReadMetrics(sparkContext)
  override lazy val metrics: Map[String, SQLMetric] = readMetrics ++ writeMetrics ++ Map(
    "dataSize" -> SQLMetrics.createSizeMetric(sparkContext, "data size"),
    "shuffle_write_rows" ->
      SQLMetrics.createMetric(sparkContext, "Native.shuffle_write_rows"),
    "shuffle_write_elapsed_compute" ->
      SQLMetrics.createNanoTimingMetric(sparkContext, "Native.shuffle_write_elapsed_compute"),
    "shuffle_read_rows" ->
      SQLMetrics.createMetric(sparkContext, "Native.shuffle_read_rows"),
    "shuffle_read_elapsed_compute" ->
      SQLMetrics.createNanoTimingMetric(sparkContext, "Native.shuffle_read_elapsed_compute"))

  private val estimatedIpcCount: Int =
    Math.max(child.outputPartitioning.numPartitions * outputPartitioning.numPartitions, 1)

  // 'mapOutputStatisticsFuture' is only needed when enable AQE.
  @transient override lazy val mapOutputStatisticsFuture: Future[MapOutputStatistics] = {
    if (inputRDD.getNumPartitions == 0) {
      Future.successful(null)
    } else {
      sparkContext
        .submitMapStage(shuffleDependency)
        .map(stat => {
          // NOTE:
          //  in the case that one ipc contains little number of records, the data size may
          //  be much more larger than unsafe row shuffle (because of a lot of redundant
          //  arrow headers). so a data size factor is needed here to prevent incorrect
          //  conversion to later SMJ/BHJ.
          //
          // assume compressed ipc size is smaller than unsafe rows only when the number of
          //  records are larger than 5
          //
          val totalShuffleRecordsWritten =
            metrics(SQLShuffleWriteMetricsReporter.SHUFFLE_RECORDS_WRITTEN).value
          val avgRecordsPerIpc = totalShuffleRecordsWritten / estimatedIpcCount
          val dataSizeFactor = Math.min(Math.max(avgRecordsPerIpc / 5.0, 0.1), 1.0)

          new MapOutputStatistics(
            stat.shuffleId,
            stat.bytesByPartitionId.map(n => (n * dataSizeFactor).ceil.toLong))
        })
    }
  }

  // If users specify the num partitions via APIs like `repartition`, we shouldn't change it.
  // For `SinglePartition`, it requires exactly one partition and we can't change it either.
  override def canChangeNumPartitions: Boolean =
    noUserSpecifiedNumPartition && outputPartitioning != SinglePartition

  override def numMappers: Int = shuffleDependency.rdd.getNumPartitions

  override def numPartitions: Int = shuffleDependency.partitioner.numPartitions

  override def getShuffleRDD(partitionSpecs: Array[ShufflePartitionSpec]): RDD[InternalRow] = {
    new ShuffledRowRDD(shuffleDependency, readMetrics, partitionSpecs)
  }

  override def runtimeStatistics: Statistics = {
    val dataSize = metrics("dataSize").value
    val rowCount = metrics(SQLShuffleWriteMetricsReporter.SHUFFLE_RECORDS_WRITTEN).value
    Statistics(dataSize, Some(rowCount))
  }

  /**
   * Caches the created ShuffleRowRDD so we can reuse that.
   */
  private var cachedShuffleRDD: ShuffledRowRDD = _

  protected override def doExecute(): RDD[InternalRow] =
    attachTree(this, "execute") {
      // Returns the same ShuffleRowRDD if this plan is used by multiple plans.
      if (cachedShuffleRDD == null) {
        cachedShuffleRDD = new ShuffledRowRDD(shuffleDependency, readMetrics)
      }
      cachedShuffleRDD
    }

  override def createNativeShuffleWriteProcessor(
      metrics: Map[String, SQLMetric]): ShuffleWriteProcessor = {

    new ShuffleWriteProcessor {
      override protected def createMetricsReporter(
          context: TaskContext): ShuffleWriteMetricsReporter = {
        new SQLShuffleWriteMetricsReporter(context.taskMetrics().shuffleWriteMetrics, metrics)
      }

      override def write(
          rdd: RDD[_],
          dep: ShuffleDependency[_, _, _],
          mapId: Long,
          context: TaskContext,
          partition: Partition): MapStatus = {

        ArrowShuffleExchangeBase.nativeShuffleWrite(
          rdd.asInstanceOf[MapPartitionsRDD[_, _]].prev.asInstanceOf[NativeRDD],
          dep,
          mapId.toInt,
          context,
          partition,
          metrics)
      }
    }
  }
}
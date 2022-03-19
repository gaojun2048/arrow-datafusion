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

use ballista::prelude::plugin_manager::global_plugin_manager;
use ballista::prelude::{BallistaConfig, BallistaContext, Result};
use datafusion::prelude::CsvReadOptions;

/// This example show the udf plugin is work
#[tokio::main]
async fn main() -> Result<()> {
    let dylib = test_cdylib::build_example("simple_udf_plugin");
    global_plugin_manager(dylib.display().to_string().as_str());
    let config = BallistaConfig::builder()
        .set("ballista.shuffle.partitions", "2")
        .build()?;
    let ctx = BallistaContext::standalone(&config, 1).await.unwrap();

    let testdata = datafusion::test_util::arrow_test_data();

    // register csv file with the execution context
    ctx.register_csv(
        "aggregate_test_100",
        &format!("{}/csv/aggregate_test_100.csv", testdata),
        CsvReadOptions::new(),
    )
    .await?;

    // execute the query
    let df = ctx.sql("show functions").await?;

    // print the results
    df.show().await?;

    Ok(())
}
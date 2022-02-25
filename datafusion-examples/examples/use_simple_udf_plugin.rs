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

use datafusion::error::Result;
use datafusion::plugin::plugin_manager::global_plugin_manager;
use datafusion::prelude::*;

/// This example test the udf plugin is work
#[tokio::main]
async fn main() -> Result<()> {
    let dylib = test_cdylib::build_example("simple_udf_plugin");
    global_plugin_manager(dylib.display().to_string().as_str());

    // create local execution context
    let mut ctx = ExecutionContext::new();

    let testdata = datafusion::test_util::arrow_test_data();

    // register csv file with the execution context
    ctx.register_csv(
        "aggregate_test_100",
        &format!("{}/csv/aggregate_test_100.csv", testdata),
        CsvReadOptions::new(),
    )
    .await?;

    // execute the query
    let df = ctx
        .sql("select array_4(1, c2, 0, 0, 2) from aggregate_test_100 limit 3")
        .await?;

    // print the results
    df.show().await?;

    Ok(())
}

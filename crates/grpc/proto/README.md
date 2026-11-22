# milvus-proto

本目录的 `.proto` 文件取自 [milvus-io/milvus-proto](https://github.com/milvus-io/milvus-proto) 的 `proto/` 目录，未做修改，许可为 Apache License 2.0（见同目录 [LICENSE](LICENSE)）。

- commit：`9e4f0ebc92af3ecf9e13c12350c6fce13d0893fa`，即 pymilvus v3.0.2 的 `pymilvus/grpc_gen/milvus-proto` 子模块所指向的版本（pymilvus 2.6.17 指向 `29edb7f`；3.0 的 `MilvusService` 是 2.6 的超集，见 [AGENTS.md「技术栈」](../../../AGENTS.md#技术栈)）。
- 文件：`milvus.proto` 及其传递依赖 `common.proto`、`schema.proto`、`feder.proto`、`msg.proto`、`rg.proto`；上游的 `tokenizer.proto` 不被引用，未收录。`google/protobuf/descriptor.proto` 由 protox 内置提供。
- 核对：`MilvusService`、`ClientTelemetryService`、`ProxyService` 的 RPC 数（144 / 4 / 1）与 pymilvus 3.0.2 的 `milvus_pb2` 一致。
- 当前测试的 pymilvus 3.0.3 / 2.6.18 未跟进。它们的 proto 相对 3.0.2 / 2.6.17 主要新增行级权限（RLS）：5 个 RPC（`MilvusService` 在 3.0.3 中为 149，在 2.6.18 中为 122）与各写入、检索请求的 `rls_principal` / `skip_rls`。3.0.3 另有 `FieldSchema.element_nullable` / `type_schema`、`ScalarField` / `VectorField.valid_data`、`SubSearchRequest.function_chains`、`FieldPartialUpdateOp` 的 `PATH_REPLACE` / `path`、`SnapshotSummary` 等。不在本 proto 中的 RPC 由 guard 返回 UNIMPLEMENTED，3.x 同样不实现；新增字段解码时忽略，3.x 也不读取。`PATH_REPLACE` 经 `field_ops` 发送，本实现照旧返回 UNIMPLEMENTED（3.x 静默按替换处理，差异已在 [0006](../../../docs/decisions/0006-m2-scope.md) 登记）。

升级时整体替换这些文件，更新上面的 commit，并重新核对 RPC 数与 pymilvus 的对应关系。生成代码由 [build.rs](../build.rs) 在编译时产生，不入库。

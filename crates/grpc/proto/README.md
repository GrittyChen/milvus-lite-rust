# milvus-proto

本目录的 `.proto` 文件取自 [milvus-io/milvus-proto](https://github.com/milvus-io/milvus-proto) 的 `proto/` 目录，未做修改，许可为 Apache License 2.0（见同目录 [LICENSE](LICENSE)）。

- commit：`9e4f0ebc92af3ecf9e13c12350c6fce13d0893fa`，即 pymilvus v3.0.2 的 `pymilvus/grpc_gen/milvus-proto` 子模块所指向的版本（pymilvus 2.6.17 指向 `29edb7f`；3.0 的 `MilvusService` 是 2.6 的超集，见 [AGENTS.md「技术栈」](../../../AGENTS.md#技术栈)）。
- 文件：`milvus.proto` 及其传递依赖 `common.proto`、`schema.proto`、`feder.proto`、`msg.proto`、`rg.proto`；上游的 `tokenizer.proto` 不被引用，未收录。`google/protobuf/descriptor.proto` 由 protox 内置提供。
- 核对：`MilvusService`、`ClientTelemetryService`、`ProxyService` 的 RPC 数（144 / 4 / 1）与 pymilvus 3.0.2 的 `milvus_pb2` 一致。

升级时整体替换这些文件，更新上面的 commit，并重新核对 RPC 数与 pymilvus 的对应关系。生成代码由 [build.rs](../build.rs) 在编译时产生，不入库。

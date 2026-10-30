# 参照实现 tests/adapter 用例清单

对照版本：milvus-lite v3.2.1（`43d1257`）的 [tests/adapter/](https://github.com/milvus-io/milvus-lite/tree/v3.2.1/tests/adapter)，47 个文件共 637 条用例。复用规则见 [0004「验收」第 2 项](../../../docs/decisions/0004-m1-scope.md#验收)及 [0006 第 25 项](../../../docs/decisions/0006-m2-scope.md#决策项)。本目录的用例复制自上游，Apache-2.0 许可；各文件保留上游的用例本体，文件头的注释写明改动。

## 状态

| 状态 | 含义 | 条数 |
| --- | --- | --- |
| 已复制 | 原样复制到本目录 | 185 |
| 改后复制 | 仅按 0004「验收」第 2 项与 0006 第 25 项授权的条件替换，逐条说明改动 | 0 |
| 范围外 | 用到当前切片不支持的功能，不复制；未实现能力的代表性入口见「范围外功能的 `UNIMPLEMENTED` 断言」 | 61 |
| 待复用 | 原功能阻断已解除，尚未复制到本地默认套件；0015 的条目已通过固定外部套件验收 | 53 |
| 依赖内部模块 | 导入或使用 3.x 的内部模块、内部对象，换成本实现后无法运行 | 33 |
| 不运行 | 上游被同名定义覆盖，上游自己也不运行 | 1 |

以上是逐条列出的 24 个文件中的 333 条；其余 23 个文件的 304 条整个文件记一行原因，见「整个文件不复用的文件」。

「已复制」与「改后复制」共 185 个用例函数，参数化展开后 214 条。S1 恢复 [test_grpc_crud.py](test_grpc_crud.py) 中的原过滤表达式，并纳入除 LIKE 外的全部 19 条；它们在本实现与真实 3.x 上的两条客户端线均作为 S1 验收。随后迁移全部 40 条待复用用例，恢复此前为绕过过滤限制而替换的原表达式；顶层动态字段切片另加入原样的 TestDynamicField 用例。0014 已恢复原 HNSW 定义，并新增创建 HNSW、DropIndex 和 describe 参数的原用例；检索仍按 FLAT 执行。

历史的 HNSW→FLAT 授权替换按 0014 恢复原定义；`== N` 换成 `in [N]` 的条件见 0004「验收」第 2 项及 0006 第 25 项。已接通 S1 的 CRUD 副本不再替换表达式或读回方式。其余副本只删去随范围外用例一起不再使用的 import、helper 与空的分节标题；上游用于跳过的 `importorskip` 改为直接导入（本仓不允许因缺依赖静默跳过），上游因 HNSW 需要 faiss 而加的 `skipif` 随 HNSW 一起去掉。起服务的 fixture 与清理见 [conftest.py](conftest.py)。

0015 切片 A 已在固定外部原始套件中实跑；本表将其中 52 个 nullable 相关函数标为待复用，说明列保留其原阻断内容，不再将该内容视为当前能力缺失。它们不计入上述 180 个已复制函数，也不增加本地默认套件；原始用例按固定 commit 在仓库外运行，详见 [A 验收](../../../docs/testing/0015-scalar-nullable-acceptance.md)。

## 逐条列出的文件

0004 最初挑出的 7 个候选文件，以及含范围内用例的其余文件，逐条列出。

### [test_grpc_collection_lifecycle.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py)（0004 候选文件）

18 条：已复制 16，范围外 2。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_create_collection_and_list`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L43) | 已复制 |  |
| [`test_create_collection_then_drop`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L48) | 已复制 |  |
| [`test_create_collection_duplicate_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L56) | 已复制 |  |
| [`test_drop_nonexistent_is_idempotent`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L63) | 已复制 |  |
| [`test_has_collection_returns_true_for_existing`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L72) | 已复制 |  |
| [`test_has_collection_returns_false_for_missing`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L77) | 已复制 |  |
| [`test_has_collection_after_drop`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L83) | 已复制 |  |
| [`test_list_collections_empty`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L93) | 已复制 |  |
| [`test_list_collections_multiple_sorted`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L97) | 已复制 |  |
| [`test_rename_collection`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L112) | 范围外 | rename |
| [`test_rename_collection_preserves_data`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L119) | 范围外 | 过滤表达式（`id == 1`）、rename |
| [`test_describe_collection_returns_schema`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L135) | 已复制 |  |
| [`test_describe_collection_field_types`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L143) | 已复制 |  |
| [`test_describe_collection_vector_dim_preserved`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L156) | 已复制 |  |
| [`test_describe_collection_varchar_max_length_preserved`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L163) | 已复制 |  |
| [`test_describe_collection_includes_partitions`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L170) | 已复制 |  |
| [`test_describe_collection_unknown_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L177) | 已复制 |  |
| [`test_dynamic_field_round_trip`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_lifecycle.py#L186) | 已复制 |  |

### [test_grpc_crud.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py)（0004 候选文件）

20 条：已复制 20。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_insert_returns_inserted_ids`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L59) | 已复制 | |
| [`test_insert_multiple_batches_accumulates`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L67) | 已复制 | |
| [`test_insert_into_unknown_collection_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L75) | 已复制 | |
| [`test_insert_round_trip_preserves_field_values`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L80) | 已复制 | |
| [`test_upsert_returns_upsert_count`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L95) | 已复制 | |
| [`test_upsert_overwrites_existing`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L102) | 已复制 | |
| [`test_query_by_id_in_list`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L120) | 已复制 | |
| [`test_query_by_id_with_output_fields`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L128) | 已复制 | |
| [`test_query_by_filter_expression`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L143) | 已复制 | |
| [`test_query_with_complex_expression`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L151) | 已复制 | |
| [`test_query_with_like_pattern`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L162) | 已复制 | 0014 切片 B，保留参照原用例 |
| [`test_query_with_no_matches`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L169) | 已复制 | |
| [`test_query_unknown_collection_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L176) | 已复制 | |
| [`test_get_by_ids`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L185) | 已复制 | |
| [`test_get_by_ids_with_output_fields`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L191) | 已复制 | |
| [`test_get_missing_ids_returns_empty`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L198) | 已复制 | |
| [`test_delete_by_ids`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L207) | 已复制 | |
| [`test_delete_idempotent`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L214) | 已复制 | |
| [`test_delete_by_filter_active_true`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L226) | 已复制 | |
| [`test_delete_by_filter_no_match`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_crud.py#L235) | 已复制 | |


### [test_grpc_search.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py)（0004 候选文件）

15 条：已复制 13，范围外 1，不运行 1。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_search_returns_top_k`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L69) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_search_multiple_queries`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L77) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_search_self_query_returns_self_top1`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L91) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_search_with_output_fields`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L105) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_search_default_output_fields_is_empty`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L117) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_search_with_filter_active_only`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L132) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_search_with_complex_filter`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L144) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_search_with_filter_no_match`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L157) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_search_after_release_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L171) | 不运行 | 被 [L184](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L184) 的同名定义覆盖，上游也不运行 |
| [`test_search_after_release_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L184) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_search_unknown_collection_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L190) | 已复制 |  |
| [`test_search_brute_force_backend`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L199) | 已复制 |  |
| [`test_search_distances_returned`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L218) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_search_results_sorted_by_cosine_score_descending`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L227) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_hnsw_search_recall_against_brute_force`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_search.py#L245) | 范围外 | 写入 1 万行，3.x 会 flush 并真正建出 HNSW |

### [test_grpc_server_startup.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_server_startup.py)（0004 候选文件）

6 条：已复制 4，依赖内部模块 2。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_server_starts_and_binds_port`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_server_startup.py#L23) | 依赖内部模块 | 断言 fixture 返回的 3.x `db` 对象（[L26](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_server_startup.py#L26)–28） |
| [`test_server_data_dir_lock_held`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_server_startup.py#L31) | 依赖内部模块 | 导入内部模块（[L34](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_server_startup.py#L34)、[L35](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_server_startup.py#L35)） |
| [`test_pymilvus_client_constructs_successfully`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_server_startup.py#L50) | 已复制 |  |
| [`test_get_version_returns_milvus_lite_string`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_server_startup.py#L57) | 已复制 |  |
| [`test_unimplemented_rpc_raises_clean_error`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_server_startup.py#L77) | 已复制 |  |
| [`test_connection_survives_unimplemented_call`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_server_startup.py#L89) | 已复制 |  |

### [test_auto_id.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py)（0004 候选文件）

12 条：已复制 4，依赖内部模块 8。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`TestAutoIdEngine::test_insert_without_pk`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L39) | 依赖内部模块 | 导入内部模块（[L27](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L27)、[L30](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L30)） |
| [`TestAutoIdEngine::test_multiple_inserts_increasing`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L52) | 依赖内部模块 | 导入内部模块（[L27](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L27)、[L30](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L30)） |
| [`TestAutoIdEngine::test_get_by_auto_id`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L64) | 依赖内部模块 | 导入内部模块（[L27](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L27)、[L30](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L30)） |
| [`TestAutoIdEngine::test_search_on_auto_id_collection`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L77) | 依赖内部模块 | 导入内部模块（[L27](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L27)、[L30](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L30)） |
| [`TestAutoIdEngine::test_insert_with_pk_still_works`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L92) | 依赖内部模块 | 导入内部模块（[L27](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L27)、[L30](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L30)） |
| [`TestAutoIdEngine::test_flush_preserves_auto_ids`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L102) | 依赖内部模块 | 导入内部模块（[L27](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L27)、[L30](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L30)） |
| [`TestAutoIdEngine::test_auto_id_only_on_int64`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L115) | 依赖内部模块 | 导入内部模块（[L117](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L117)、[L120](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L120)、[L127](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L127)） |
| [`TestAutoIdEngine::test_num_entities_with_auto_id`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L130) | 依赖内部模块 | 导入内部模块（[L27](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L27)、[L30](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L30)） |
| [`test_grpc_auto_id_create_and_insert`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L146) | 已复制 |  |
| [`test_grpc_auto_id_search`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L168) | 已复制 |  |
| [`test_grpc_auto_id_query`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L195) | 已复制 |  |
| [`test_grpc_auto_id_ids_increasing`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_auto_id.py#L225) | 已复制 |  |

### [test_grpc_quickstart.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_quickstart.py)（0004 候选文件）

2 条：已复制 1，范围外 1。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_pymilvus_quickstart`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_quickstart.py#L26) | 范围外 | 过滤表达式（`score > 0.5`）、delete、分区、flush、HNSW |
| [`test_quickstart_multi_query_search`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_quickstart.py#L146) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |

### [test_grpc_error_mapping.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py)（0004 候选文件）

9 条：已复制 3，范围外 1，依赖内部模块 5。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_collection_not_found_dual_codes`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py#L32) | 依赖内部模块 | 直接调用 3.x 错误映射，[L14](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py#L14)–25 顶层导入 |
| [`test_collection_not_loaded_code`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py#L40) | 依赖内部模块 | 直接调用 3.x 错误映射，[L14](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py#L14)–25 顶层导入 |
| [`test_schema_validation_code`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py#L47) | 依赖内部模块 | 直接调用 3.x 错误映射，[L14](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py#L14)–25 顶层导入 |
| [`test_unknown_exception_maps_to_unexpected`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py#L54) | 依赖内部模块 | 直接调用 3.x 错误映射，[L14](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py#L14)–25 顶层导入 |
| [`test_drop_nonexistent_is_idempotent`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py#L65) | 已复制 |  |
| [`test_search_after_release_reports_not_loaded`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py#L70) | 已复制 |  |
| [`test_rename_nonexistent_reports_not_found`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py#L92) | 范围外 | rename |
| [`test_hybrid_search_unimplemented`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py#L99) | 依赖内部模块 | 导入内部模块（[L104](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py#L104)） |
| [`test_create_alias_nonexistent_collection_reports_not_found`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_error_mapping.py#L108) | 已复制 |  |

### [test_crud_alignment.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment.py)

7 条：已复制 7。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_delete_by_filter_without_load`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment.py#L41) | 已复制 |  |
| [`test_query_count_star`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment.py#L65) | 已复制 |  |
| [`test_query_count_star_with_filter`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment.py#L76) | 已复制 |  |
| [`test_query_count_star_no_filter`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment.py#L86) | 已复制 |  |
| [`test_get_with_output_fields`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment.py#L100) | 已复制 |  |
| [`test_search_round_decimal`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment.py#L119) | 已复制 |  |
| [`test_query_wildcard_output_fields`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment.py#L139) | 已复制 |  |

### [test_crud_alignment_milvus.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment_milvus.py)

6 条：已复制 5，范围外 1。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_delete_with_filters`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment_milvus.py#L90) | 已复制 |  |
| [`test_delete_with_filters_nullable_vector_field`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment_milvus.py#L142) | 范围外 | 过滤表达式（`id in {ids_to_delete}`）、delete、nullable |
| [`test_count_with_expr`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment_milvus.py#L208) | 已复制 |  |
| [`test_get_output_fields`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment_milvus.py#L242) | 已复制 |  |
| [`test_query_output_fields_all`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment_milvus.py#L280) | 已复制 |  |
| [`test_search_round_decimal`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_crud_alignment_milvus.py#L325) | 已复制 |  |

### [test_dynamic_field_coverage.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_dynamic_field_coverage.py)

8 条：已复制 5，范围外 3。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_dynamic_get_projection`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_dynamic_field_coverage.py#L88) | 已复制 |  |
| [`test_dynamic_wildcard_preserves_all_json_types`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_dynamic_field_coverage.py#L101) | 已复制 |  |
| [`test_dynamic_path_and_array_filters_through_grpc`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_dynamic_field_coverage.py#L132) | 已复制 | 0014 切片 B，保留参照原用例 |
| [`test_dynamic_multi_query_search_with_meta_projection`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_dynamic_field_coverage.py#L160) | 已复制 |  |
| [`test_dynamic_zero_hit_search_with_meta_projection`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_dynamic_field_coverage.py#L180) | 已复制 | 0014 切片 B，保留参照原用例 |
| [`test_dynamic_star_survives_grpc_group_by`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_dynamic_field_coverage.py#L198) | 范围外 | group_by |
| [`test_dynamic_star_survives_grpc_ranker`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_dynamic_field_coverage.py#L218) | 范围外 | rerank |
| [`test_dynamic_star_survives_hybrid_search_group_by`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_dynamic_field_coverage.py#L246) | 范围外 | hybrid search、rerank、group_by |

### [test_function_chain.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py)

22 条：已复制 1，范围外 15，依赖内部模块 6。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_search_function_chain_reranks_by_hidden_field_and_serializes_score`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L188) | 范围外 | rerank、function chain |
| [`test_search_function_chain_preserves_explicit_dynamic_output_field`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L214) | 范围外 | function chain |
| [`test_search_function_chain_preserves_default_dynamic_output_fields`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L237) | 范围外 | function chain |
| [`test_search_function_chain_preserves_explicit_dynamic_field_on_temp_collision`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L259) | 范围外 | function chain |
| [`test_search_function_chain_preserves_default_dynamic_field_on_temp_collision`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L281) | 范围外 | function chain |
| [`test_search_function_chain_uses_primary_field_name_as_id_alias`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L302) | 依赖内部模块 | 导入内部模块（[L307](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L307)） |
| [`test_search_function_chain_limit_changes_each_query_chunk`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L348) | 范围外 | function chain |
| [`test_search_function_chain_keeps_query_chunks_independent`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L372) | 范围外 | function chain |
| [`test_search_function_chain_without_sort_preserves_ann_order`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L397) | 范围外 | function chain |
| [`test_search_function_chain_explicit_id_tie_break`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L416) | 范围外 | function chain |
| [`test_search_function_chain_rewrites_metric_scores`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L446) | 范围外 | function chain |
| [`test_public_chain_keeps_requested_candidate_budget`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L479) | 依赖内部模块 | 导入内部模块（[L483](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L483)） |
| [`test_public_chain_preserves_group_by_response_metadata`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L518) | 范围外 | group_by、function chain |
| [`test_raw_search_rejects_function_score_function_chain_conflict`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L544) | 范围外 | function chain |
| [`test_raw_search_rejects_order_by_function_chain_conflict_before_ann`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L567) | 依赖内部模块 | 导入内部模块（[L572](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L572)） |
| [`test_raw_search_rejects_unknown_rerank_provider_as_illegal_argument`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L601) | 范围外 | function chain、rerank |
| [`test_raw_search_rejects_request_rerank_endpoint_as_illegal_argument`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L629) | 范围外 | function chain、rerank |
| [`test_raw_search_rejects_unsupported_rerank_param_before_ann`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L658) | 依赖内部模块 | 导入内部模块（[L663](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L663)） |
| [`test_raw_search_rejects_primary_name_conflicting_with_system_column`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L696) | 依赖内部模块 | 导入内部模块（[L701](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L701)、[L702](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L702)、[L705](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L705)、[L706](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L706)） |
| [`test_raw_hybrid_search_rejects_function_chains`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L751) | 范围外 | hybrid search、function chain |
| [`test_search_without_function_chain_preserves_legacy_behavior`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L770) | 已复制 |  |
| [`test_parse_search_request_accepts_legacy_shape_without_new_messages`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L794) | 依赖内部模块 | 调用 [L13](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_function_chain.py#L13) 顶层导入的 `parse_search_request` |

### [test_grpc_index.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py)

18 条：已复制 17，待复用 1。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_create_index_hnsw`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L66) | 已复制 | 0014 接入索引定义与 DropIndex；错误码由差分及翻译测试补充验证 |
| [`test_create_index_brute_force`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L73) | 已复制 |  |
| [`test_create_index_duplicate_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L80) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_create_index_unknown_collection_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L87) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_create_index_metric_round_trip`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L92) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_drop_index`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L101) | 已复制 | 0014 接入索引定义与 DropIndex；错误码由差分及翻译测试补充验证 |
| [`test_drop_index_unknown_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L111) | 已复制 | 0014 接入索引定义与 DropIndex；错误码由差分及翻译测试补充验证 |
| [`test_load_state_before_create_index`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L122) | 已复制 |  |
| [`test_load_state_after_create_index_is_loaded`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L131) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_load_collection`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L137) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_release_collection`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L143) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_load_release_cycle`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L150) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_get_load_state_unknown_collection`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L158) | 已复制 |  |
| [`test_describe_index_returns_params`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L169) | 已复制 | 0014 接入索引定义与 DropIndex；错误码由差分及翻译测试补充验证 |
| [`test_describe_index_state_is_finished`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L176) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_describe_index_name_matches_field_name`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L183) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`test_describe_index_empty_when_no_index`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L190) | 已复制 |  |
| [`test_create_scalar_inverted_index_query_and_search`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_index.py#L196) | 待复用 | 过滤表达式（`age >= 20`）、flush、标量索引、nullable |

### [test_grpc_show_in_memory.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_show_in_memory.py)

6 条：已复制 3，范围外 3。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_show_collections_in_memory_reports_loaded_percentage`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_show_in_memory.py#L37) | 已复制 |  |
| [`test_show_collections_in_memory_reports_released_percentage`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_show_in_memory.py#L55) | 已复制 |  |
| [`test_show_collections_in_memory_filters_requested_names`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_show_in_memory.py#L75) | 已复制 |  |
| [`test_show_partitions_in_memory_reports_collection_load_percentage`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_show_in_memory.py#L92) | 范围外 | 分区 |
| [`test_show_partitions_in_memory_reports_released_collection`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_show_in_memory.py#L109) | 范围外 | 分区 |
| [`test_show_partitions_in_memory_filters_requested_names`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_show_in_memory.py#L128) | 范围外 | 分区 |

### [test_issue_19_20.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py)

10 条：已复制 6，依赖内部模块 4。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`TestDynamicFieldTypes::test_dynamic_int_value_returned`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py#L13) | 已复制 |  |
| [`TestDynamicFieldTypes::test_dynamic_float_value_returned`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py#L30) | 已复制 |  |
| [`TestDynamicFieldTypes::test_dynamic_bool_value_returned`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py#L47) | 已复制 |  |
| [`TestDynamicFieldTypes::test_dynamic_mixed_types`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py#L63) | 已复制 |  |
| [`TestDynamicFieldTypes::test_dynamic_list_value_returned`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py#L84) | 已复制 |  |
| [`TestDynamicFieldTypes::test_dynamic_dict_value_returned`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py#L101) | 已复制 |  |
| [`TestJiebaAnalyzerKey::test_jieba_via_type_key`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py#L122) | 依赖内部模块 | 导入内部模块（[L125](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py#L125)） |
| [`TestJiebaAnalyzerKey::test_jieba_via_tokenizer_key_still_works`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py#L129) | 依赖内部模块 | 导入内部模块（[L132](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py#L132)） |
| [`TestJiebaAnalyzerKey::test_standard_via_type_key`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py#L136) | 依赖内部模块 | 导入内部模块（[L137](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py#L137)） |
| [`TestJiebaAnalyzerKey::test_default_without_key`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py#L141) | 依赖内部模块 | 导入内部模块（[L142](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_19_20.py#L142)） |

### [test_issue_fixes.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py)

9 条：已复制 5，范围外 2，依赖内部模块 2。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_dynamic_field_query_output`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py#L19) | 已复制 |  |
| [`test_multi_vector_fields`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py#L56) | 范围外 | 多个向量字段 |
| [`test_ip_distance_sign`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py#L92) | 依赖内部模块 | 导入内部模块（[L94](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py#L94)、[L95](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py#L95)、[L96](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py#L96)） |
| [`test_grpc_large_batch_insert`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py#L124) | 已复制 |  |
| [`test_describe_index_by_field_name`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py#L146) | 已复制 |  |
| [`test_autoindex_uses_hnsw`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py#L169) | 依赖内部模块 | 导入内部模块（[L171](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py#L171)、[L172](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py#L172)） |
| [`test_raw_distance_semantics_match_milvus_server`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py#L190) | 已复制 |  |
| [`test_dynamic_wildcard_and_meta_search_output`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py#L266) | 已复制 |  |
| [`test_dynamic_array_filter_search`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_issue_fixes.py#L292) | 待复用 | 原过滤阻断已解除，固定外部套件通过；尚未复制 |

### [test_milvus_compat.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py)

40 条：已复制 30，范围外 9，待复用 1。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`TestInsert::test_insert_and_count`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L88) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestInsert::test_insert_string_pk`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L94) | 已复制 |  |
| [`TestInsert::test_insert_multiple_batches`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L104) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestSearch::test_search_basic`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L127) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestSearch::test_search_self_query_l2`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L135) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestSearch::test_search_metric_types`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L145) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestSearch::test_search_with_filter_int`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L152) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestSearch::test_search_with_filter_bool`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L164) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestSearch::test_search_with_filter_varchar_like`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L176) | 范围外 | 过滤表达式（`varchar like "str_01%"`）、HNSW |
| [`TestSearch::test_search_with_filter_and_or`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L188) | 范围外 | 过滤表达式（`id >= 100 and (bool_field == true or float_field < 50.0)`）、HNSW |
| [`TestSearch::test_search_with_filter_in`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L202) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestSearch::test_search_with_filter_not`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L215) | 范围外 | 过滤表达式（`not (id < 100)`）、HNSW |
| [`TestSearch::test_search_multi_query`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L227) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestSearch::test_search_with_output_fields`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L237) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestSearch::test_search_results_sorted_by_cosine_score_descending`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L249) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestQuery::test_query_all`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L263) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestQuery::test_query_with_limit`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L269) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestQuery::test_query_id_in_list`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L275) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestQuery::test_query_range_expression`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L285) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestQuery::test_query_varchar_like_prefix`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L296) | 范围外 | 过滤表达式（`varchar like "str_00%"`）、HNSW |
| [`TestQuery::test_query_bool_filter`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L308) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestQuery::test_query_not_in`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L319) | 范围外 | 过滤表达式（`id not in [0, 1, 2]`）、HNSW |
| [`TestQuery::test_get_by_ids`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L332) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestQuery::test_get_missing_ids_returns_empty`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L338) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestQuery::test_query_output_fields_subset`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L344) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestDelete::test_delete_by_ids`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L361) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestDelete::test_delete_by_filter`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L370) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestDelete::test_delete_then_search_excludes_deleted`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L379) | 范围外 | delete、HNSW |
| [`TestDelete::test_delete_nonexistent_pk_no_error`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L388) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestUpsert::test_upsert_existing_pk_overwrites`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L401) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestUpsert::test_upsert_new_pk_inserts`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L418) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestUpsert::test_upsert_multiple_times`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L435) | 已复制 | 恢复 HNSW 原定义（0014，按 FLAT 执行，无需 faiss） |
| [`TestPartition::test_partition_lifecycle`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L459) | 范围外 | 分区 |
| [`TestPartition::test_partition_insert_and_search_scoped`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L468) | 范围外 | 分区、HNSW |
| [`TestIndexLifecycle::test_index_create_load_release_drop_cycle`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L507) | 范围外 | DropIndex；断言索引类型为 HNSW |
| [`TestDynamicField::test_dynamic_field_insert_and_query`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L551) | 已复制 | 顶层动态字段过滤，函数体保持原文 |
| [`TestNullableField::test_nullable_varchar_insert_and_query`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L585) | 待复用 | 过滤表达式（`title like "t%"`）、nullable |
| [`TestCollectionLifecycle::test_create_describe_drop`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L620) | 已复制 |  |
| [`TestCollectionLifecycle::test_create_duplicate_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L628) | 已复制 |  |
| [`TestCollectionLifecycle::test_list_multiple_collections`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat.py#L634) | 已复制 |  |

### [test_milvus_compat_crud.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py)

52 条：已复制 4，范围外 5，待复用 43。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`TestInsert::test_insert_basic`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L75) | 待复用 | 过滤表达式（`id >= 0`）、nullable |
| [`TestInsert::test_insert_empty_list`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L80) | 待复用 | 过滤表达式（`id >= 0`）、nullable |
| [`TestInsert::test_insert_missing_vector_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L85) | 待复用 | nullable |
| [`TestInsert::test_insert_dim_mismatch_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L90) | 待复用 | nullable |
| [`TestInsert::test_insert_extra_field_no_dynamic_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L95) | 待复用 | nullable |
| [`TestInsert::test_insert_with_dynamic_field`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L102) | 待复用 | 过滤表达式（`id == 1`）、nullable |
| [`TestInsert::test_insert_string_pk`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L111) | 已复制 |  |
| [`TestUpsert::test_upsert_as_insert`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L130) | 待复用 | 过滤表达式（`id >= 0`）、upsert、nullable |
| [`TestUpsert::test_upsert_overwrites_existing`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L140) | 待复用 | 过滤表达式（`id == 1`）、upsert、nullable |
| [`TestUpsert::test_upsert_multiple_times`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L152) | 待复用 | 过滤表达式（`id == 1`）、upsert、nullable |
| [`TestUpsert::test_upsert_with_dynamic_field`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L163) | 待复用 | 过滤表达式（`id == 1`）、upsert、nullable |
| [`TestUpsert::test_upsert_duplicate_pk_in_batch`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L175) | 待复用 | 过滤表达式（`id == 1`）、upsert、nullable |
| [`TestDelete::test_delete_by_ids`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L192) | 待复用 | delete、nullable |
| [`TestDelete::test_delete_by_filter`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L198) | 待复用 | 过滤表达式（`id < 10`）、delete、nullable |
| [`TestDelete::test_delete_then_search_excludes`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L204) | 待复用 | delete、nullable |
| [`TestDelete::test_delete_all_then_query_empty`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L212) | 待复用 | 过滤表达式（`id >= 0`）、delete、nullable |
| [`TestQuery::test_query_default`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L224) | 待复用 | 过滤表达式（`id >= 0`）、nullable |
| [`TestQuery::test_query_output_fields`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L229) | 待复用 | 过滤表达式（`id == 0`）、nullable |
| [`TestQuery::test_query_output_fields_wildcard`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L236) | 待复用 | 过滤表达式（`id == 0`）、nullable |
| [`TestQuery::test_query_empty_collection`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L244) | 待复用 | 过滤表达式（`id >= 0`）、nullable |
| [`TestQuery::test_query_empty_in_list`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L249) | 待复用 | nullable |
| [`TestQuery::test_query_not_in`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L255) | 待复用 | 过滤表达式（`id not in [0, 1]`）、nullable |
| [`TestQuery::test_query_not_in_empty_returns_all`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L262) | 待复用 | 过滤表达式（`id not in []`）、nullable |
| [`TestQuery::test_query_with_limit`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L269) | 待复用 | 过滤表达式（`id >= 0`）、nullable |
| [`TestQuery::test_query_pagination`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L274) | 待复用 | 过滤表达式（`id >= 0`）、nullable |
| [`TestQuery::test_query_offset_beyond_total`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L286) | 待复用 | 过滤表达式（`id >= 0`）、nullable |
| [`TestQuery::test_query_by_bool_field`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L292) | 待复用 | 过滤表达式（`active == true`）、nullable |
| [`TestQuery::test_query_by_float_field`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L299) | 待复用 | 过滤表达式（`score > 0.5`）、nullable |
| [`TestQuery::test_query_multi_logical`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L306) | 待复用 | 过滤表达式（`id >= 10 and id < 20 and active == true`）、nullable |
| [`TestQuery::test_query_like`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L316) | 待复用 | 过滤表达式（`title like "doc_1%"`）、nullable |
| [`TestQuery::test_query_dup_pk_returns_latest`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L323) | 待复用 | 过滤表达式（`id == 1`）、nullable |
| [`TestQuery::test_query_without_loading_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L335) | 待复用 | 过滤表达式（`id >= 0`）、nullable |
| [`TestQuery::test_query_not_exist_collection_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L346) | 已复制 |  |
| [`TestGet::test_get_basic`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L357) | 待复用 | nullable |
| [`TestGet::test_get_nonexistent_ids`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L362) | 待复用 | nullable |
| [`TestGet::test_get_output_fields`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L367) | 待复用 | nullable |
| [`TestSearch::test_search_basic`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L379) | 待复用 | nullable |
| [`TestSearch::test_search_with_filter`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L388) | 待复用 | 过滤表达式（`id >= 100`）、nullable |
| [`TestSearch::test_search_output_fields`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L396) | 待复用 | nullable |
| [`TestSearch::test_search_l2_metric`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L405) | 待复用 | nullable |
| [`TestSearch::test_search_offset`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L417) | 待复用 | nullable |
| [`TestSearch::test_search_batch_queries`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L428) | 待复用 | nullable |
| [`TestSearch::test_search_string_pk`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L437) | 已复制 |  |
| [`TestSearch::test_search_nonexistent_collection_raises`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L451) | 已复制 |  |
| [`TestPartition::test_query_specific_partition`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L461) | 范围外 | 过滤表达式（`id >= 0`）、分区、nullable |
| [`TestPartition::test_query_multiple_partitions`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L480) | 范围外 | 过滤表达式（`id >= 0`）、分区、nullable |
| [`TestPartition::test_query_empty_partition`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L498) | 范围外 | 过滤表达式（`id >= 0`）、分区、nullable |
| [`TestDefaultValue::test_insert_with_default_value`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L515) | 范围外 | 过滤表达式（`id == 1`）、default value |
| [`TestDefaultValue::test_insert_override_default_value`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L536) | 范围外 | 过滤表达式（`id == 1`）、default value |
| [`TestNullable::test_insert_null_fields`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L560) | 待复用 | 过滤表达式（`id == 1`）、nullable |
| [`TestNullable::test_query_is_null`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L569) | 待复用 | 过滤表达式（`title is null`）、nullable |
| [`TestNullable::test_query_is_not_null`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_crud.py#L578) | 待复用 | 过滤表达式（`title is not null`）、nullable |

### [test_milvus_compat_search.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py)

16 条：已复制 3，范围外 9，待复用 4。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`TestSearchJSON::test_search_json_comparison`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L53) | 范围外 | 过滤表达式（`info["number"] >= 50`） |
| [`TestSearchJSON::test_search_json_nested`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L60) | 范围外 | 过滤表达式（`info["nested"]["a"] >= 500`） |
| [`TestSearchJSON::test_search_json_string_eq`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L67) | 范围外 | 过滤表达式（`info["tag"] == "cat_0"`） |
| [`TestSearchJSON::test_query_json_nested_like`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L75) | 范围外 | 过滤表达式（`info["nested"]["b"] like "nested_1%"`） |
| [`TestSearchPagination::test_paginated_search_matches_full`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L105) | 已复制 |  |
| [`TestSearchPagination::test_search_offset_beyond_total`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L116) | 已复制 |  |
| [`TestSearchPagination::test_search_pagination_with_filter`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L121) | 已复制 |  |
| [`TestSearchPagination::test_search_pagination_in_partition`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L134) | 范围外 | 分区 |
| [`TestSearchTextMatch::test_search_with_text_match_single_token`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L180) | 范围外 | 过滤表达式（`text_match(text, "python")`）、全文检索 |
| [`TestSearchTextMatch::test_search_with_text_match_multi_token`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L188) | 范围外 | 过滤表达式（`text_match(text, "programming")`）、全文检索 |
| [`TestSearchTextMatch::test_query_with_text_match`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L196) | 范围外 | 过滤表达式（`text_match(text, "learning")`）、全文检索 |
| [`TestSearchTextMatch::test_search_text_match_combined_with_scalar`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L203) | 范围外 | 过滤表达式（`text_match(text, "learning") and id >= 4`）、全文检索 |
| [`TestUpsertViaGRPC::test_upsert_overwrites`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L239) | 待复用 | 过滤表达式（`id == 1`）、upsert、nullable |
| [`TestUpsertViaGRPC::test_upsert_new_pk_as_insert`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L250) | 待复用 | 过滤表达式（`id == 3`）、upsert、nullable |
| [`TestUpsertViaGRPC::test_upsert_then_search`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L258) | 待复用 | upsert、nullable |
| [`TestUpsertViaGRPC::test_upsert_multiple_times`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_search.py#L270) | 待复用 | 过滤表达式（`id == 1`）、upsert、nullable |

### [test_milvus_compat_v3.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py)

26 条：已复制 22，范围外 1，待复用 3。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`TestVarcharPK::test_search_varchar_pk_equality`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L129) | 已复制 |  |
| [`TestVarcharPK::test_query_varchar_pk_like_prefix`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L147) | 已复制 | 0014 切片 B，保留参照原用例 |
| [`TestVarcharPK::test_upsert_varchar_pk`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L165) | 已复制 |  |
| [`TestPaginationConsistency::test_paginated_search_matches_full`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L195) | 已复制 |  |
| [`TestPaginationConsistency::test_offset_beyond_count_returns_empty`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L222) | 已复制 |  |
| [`TestPaginationConsistency::test_search_pagination_with_filter`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L235) | 已复制 |  |
| [`TestPartitionPagination::test_search_in_partition_returns_only_partition_data`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L270) | 范围外 | 分区 |
| [`TestJsonFieldFilters::test_query_json_nested_field`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L330) | 已复制 | 0014 切片 B，保留参照原用例 |
| [`TestJsonFieldFilters::test_search_json_string_filter`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L346) | 已复制 | 0014 切片 B，保留参照原用例 |
| [`TestMultiNullableTypes::test_query_multi_null_checks`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L398) | 待复用 | 过滤表达式（`int_val is null and text is not null`）、nullable |
| [`TestMultiNullableTypes::test_query_or_with_nullable`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L419) | 待复用 | 过滤表达式（`int_val is null or float_val > 2.0`）、nullable |
| [`TestStringInFilter::test_search_varchar_in_list`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L452) | 已复制 | 0014 切片 B，保留参照原用例 |
| [`TestStringInFilter::test_query_varchar_not_in`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L467) | 已复制 | 0014 切片 B，保留参照原用例 |
| [`TestMixedFilter::test_search_mixed_int_varchar`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L489) | 已复制 |  |
| [`TestDistanceOrdering::test_cosine_scores_descending`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L512) | 已复制 |  |
| [`TestDistanceOrdering::test_self_query_cosine_score_near_one`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L523) | 已复制 |  |
| [`TestDeleteAllNullable::test_delete_all_then_verify_empty`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L541) | 待复用 | 过滤表达式（`id >= 0`）、delete、nullable |
| [`TestChainedRange::test_query_range_both_bounds`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L590) | 已复制 |  |
| [`TestGetWithOutputFields::test_get_explicit_fields`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L611) | 已复制 |  |
| [`TestFilterZeroMatch::test_search_filter_no_match`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L631) | 已复制 |  |
| [`TestFilterZeroMatch::test_query_filter_no_match`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L643) | 已复制 |  |
| [`TestAutoIdViaGrpc::test_auto_id_insert_returns_ids`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L660) | 已复制 |  |
| [`TestMultiQuerySearch::test_multi_nq_self_query`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L698) | 已复制 |  |
| [`TestVarcharEmptyComparison::test_varchar_gte_empty_matches_all`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L718) | 已复制 |  |
| [`TestGetPartialMiss::test_get_mixed_existing_and_missing`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L736) | 已复制 |  |
| [`TestGetPartialMiss::test_get_all_missing`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v3.py#L744) | 已复制 |  |

### [test_milvus_lite_compat.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_lite_compat.py)

7 条：已复制 6，依赖内部模块 1。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`TestServerManager::test_start_and_get_uri`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_lite_compat.py#L23) | 已复制 |  |
| [`TestServerManager::test_reuse_same_path`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_lite_compat.py#L36) | 已复制 |  |
| [`TestServerManager::test_different_paths`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_lite_compat.py#L47) | 已复制 |  |
| [`TestServerManager::test_release_server`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_lite_compat.py#L57) | 依赖内部模块 | 断言私有属性 `mgr._servers`（[L63](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_lite_compat.py#L63)） |
| [`TestPymilvusIntegration::test_full_lifecycle_via_server_manager`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_lite_compat.py#L69) | 已复制 |  |
| [`TestPymilvusIntegration::test_shorthand_create_collection`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_lite_compat.py#L134) | 已复制 |  |
| [`TestPymilvusIntegration::test_dot_db_uri_via_pymilvus`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_lite_compat.py#L155) | 已复制 |  |

### [test_offset.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py)

8 条：已复制 3，依赖内部模块 5。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`TestOffsetEngine::test_search_offset_skips_top`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L42) | 依赖内部模块 | 导入内部模块（[L26](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L26)、[L29](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L29)） |
| [`TestOffsetEngine::test_search_offset_zero_is_default`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L56) | 依赖内部模块 | 导入内部模块（[L26](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L26)、[L29](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L29)） |
| [`TestOffsetEngine::test_search_offset_beyond_count`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L64) | 依赖内部模块 | 导入内部模块（[L26](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L26)、[L29](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L29)） |
| [`TestOffsetEngine::test_query_offset`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L71) | 依赖内部模块 | 导入内部模块（[L26](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L26)、[L29](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L29)） |
| [`TestOffsetEngine::test_query_offset_without_limit`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L83) | 依赖内部模块 | 导入内部模块（[L26](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L26)、[L29](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L29)） |
| [`test_grpc_search_offset`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L111) | 已复制 |  |
| [`test_grpc_query_offset`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L124) | 已复制 |  |
| [`test_grpc_search_offset_empty`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_offset.py#L135) | 已复制 |  |

### [test_p0_features.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_p0_features.py)

10 条：已复制 2，范围外 8。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_alias_lifecycle_and_alias_reads`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_p0_features.py#L16) | 已复制 |  |
| [`test_truncate_collection_preserves_schema`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_p0_features.py#L49) | 范围外 | 过滤表达式（`id == 2`）、truncate |
| [`test_list_indexes_returns_index_names`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_p0_features.py#L68) | 已复制 |  |
| [`test_get_partition_stats`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_p0_features.py#L82) | 范围外 | 分区 |
| [`test_truncate_preserves_metadata_and_clears_mutation_history`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_p0_features.py#L103) | 范围外 | 过滤表达式（`id >= 0`）、delete、upsert、分区、alias、修改 collection、truncate |
| [`test_truncate_released_collection_then_flush`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_p0_features.py#L156) | 范围外 | flush、truncate |
| [`test_truncate_is_database_isolated`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_p0_features.py#L172) | 范围外 | 多数据库、truncate |
| [`test_collection_stats_follow_upsert_delete_flush_and_truncate`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_p0_features.py#L195) | 范围外 | delete、upsert、flush、truncate |
| [`test_partition_stats_follow_upsert_delete_and_flush`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_p0_features.py#L231) | 范围外 | delete、upsert、分区、flush |
| [`test_statistics_missing_resources_raise`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_p0_features.py#L273) | 范围外 | 分区 |

### [test_grpc_collection_properties.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_properties.py)

4 条：已复制 3，范围外 1。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| [`test_collection_properties_set_replace_and_delete`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_properties.py#L23) | 已复制 |  |
| [`test_alter_missing_collection_has_no_side_effect`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_properties.py#L42) | 已复制 |  |
| [`test_collection_properties_via_alias`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_properties.py#L53) | 已复制 |  |
| [`test_collection_properties_are_database_isolated`](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_collection_properties.py#L68) | 范围外 | 多数据库 |

## [test_grpc_compaction.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_compaction.py)

2 条原样复制。自动维护及两个应答 RPC 的合同见 [0010](../../../docs/decisions/0010-compaction-and-tombstone-gc.md)；Completed 是兼容应答，不表示后台维护已排空。

| 用例 | 状态 | 说明 |
| --- | --- | --- |
| `test_compact_empty_collection_reports_completed` | 已复制 | 空集合应答 |
| `test_compact_preserves_logical_rows_and_search` | 已复制 | 更新、删除与检索结果保持 |

## 范围外功能的 `UNIMPLEMENTED` 断言

范围外的用例不复制，也不用 skip 占位。下表保留 M1 当时的断言分类；0014 / 0015 / 0016 已接入的功能不再 UNIMPLEMENTED，当前范围以相应决策和测试为准：

| 功能 | 用例 |
| --- | --- |
| 0014 之外的过滤构造（算术、时间、text_match 与几何） | [test_grpc_translation.py](../test_grpc_translation.py) 的 `test_search_m1_unsupported`、`test_query_m1_unsupported`、`test_pymilvus_entry_points_remaining_unsupported` |
| Query / search iterator V1 | 0016 A / B 接续；V2 不增加服务端实现 |
| 分区 RPC；写入、检索、查询指定 `_default` 以外的分区；partition key | `test_insert_m1_unsupported`、`test_search_m1_unsupported`、`test_query_m1_unsupported`、`test_pymilvus_entry_points_remaining_unsupported` |
| 数据库 RPC | `test_pymilvus_entry_points_remaining_unsupported` |
| schema：GEOMETRY、TIMESTAMPTZ、稀疏向量、nullable 向量、向量默认值、JSON/嵌套 ARRAY 元素、function、多向量字段、analyzer，properties 中的 `timezone` | `test_create_collection_remaining_unsupported`、`test_pymilvus_entry_points_remaining_unsupported` |
| 写入 null 值 | `test_insert_m1_unsupported` |
| 修改 schema、rename | `test_pymilvus_entry_points_remaining_unsupported` |
| HybridSearch、group_by、rerank、请求中的 `timezone` | `test_search_m1_unsupported`、`test_pymilvus_entry_points_remaining_unsupported` |
| FLAT、BRUTE_FORCE、AUTOINDEX 以外的索引类型 | `test_create_index_m1_unsupported` |
| DropIndex | `test_pymilvus_entry_points_remaining_unsupported` |
| GetCompactionPlans | [test_server_manager.py](../test_server_manager.py) 的 `test_unimplemented_rpc_reason` |
| snapshot 等其余未列出的 RPC 与 service | `test_unimplemented_rpc_reason`（未实现的 RPC 统一由 [guard.rs](../../../crates/grpc/src/guard.rs) 处理） |

## 整个文件不复用的文件

以下文件没有 M1 范围内的用例，整个文件记一行原因。

| 文件 | 用例 | 原因 |
| --- | --- | --- |
| [test_against_milvus.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_against_milvus.py) | 17 | 连接真实的 Milvus 服务（`MILVUS_URI`），不针对 milvus-lite |
| [test_array_field.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_array_field.py) | 9 | 0015 B 后 gRPC 用例待复用；其中 7 条依赖内部模块，仍不直接复制 |
| [test_array_milvus.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_array_milvus.py) | 22 | 0015 B 已通过固定外部套件，待复用 |
| [test_boost_ranker.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_boost_ranker.py) | 8 | rerank（boost） |
| [test_group_by.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_group_by.py) | 11 | group_by；其中 6 条依赖内部模块 |
| [test_grpc_database.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_database.py) | 23 | 多数据库 |
| [test_grpc_fts.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_fts.py) | 7 | BM25、稀疏向量、text_match；其中 1 条依赖内部模块 |
| [test_grpc_geometry.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_geometry.py) | 8 | GEOMETRY |
| [test_grpc_partition_key.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_partition_key.py) | 6 | partition key |
| [test_grpc_snapshot.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_snapshot.py) | 10 | snapshot；全部依赖内部模块：经 fixture 返回的 3.x `db` 对象创建 snapshot，[L11](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_snapshot.py#L11) 顶层导入 |
| [test_grpc_timestamptz_milvus_compat.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_timestamptz_milvus_compat.py) | 16 | TIMESTAMPTZ、default value |
| [test_grpc_translators_records.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_translators_records.py) | 26 | 全部依赖内部模块：3.x 翻译层的单元测试，[L21](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_translators_records.py#L21)–28 顶层导入 |
| [test_grpc_translators_result.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_translators_result.py) | 1 | 依赖内部模块：3.x 翻译层的单元测试，[L3](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_translators_result.py#L3)–5 顶层导入 |
| [test_grpc_translators_schema.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_translators_schema.py) | 16 | 全部依赖内部模块：3.x 翻译层的单元测试，[L14](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_grpc_translators_schema.py#L14)–19 顶层导入 |
| [test_hybrid_search.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_hybrid_search.py) | 19 | hybrid search、rerank、BM25；其中 11 条依赖内部模块 |
| [test_iterator.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_iterator.py) | 11 | 6 条 Query iterator 由 0016 在固定外部原始套件实跑，待复用；1 条 search iterator V1 由 0016 B 接续；4 条依赖内部模块 |
| [test_json_field.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_json_field.py) | 21 | 共享 fixture 的 JSON 字段为 nullable，并写入了 null 值（[L18](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_json_field.py#L18)、L37）；多数用例还用 JSON 过滤表达式 |
| [test_milvus_compat_partition_key.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_partition_key.py) | 9 | partition key；其中 3 条只断言抛出异常，`UNIMPLEMENTED` 也会通过 |
| [test_milvus_compat_v2.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_compat_v2.py) | 24 | 全部用到过滤表达式，多数还有 nullable 与 HNSW |
| [test_milvus_fts_compat.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_milvus_fts_compat.py) | 14 | BM25、text_match、nullable；其中 8 条依赖内部模块 |
| [test_multi_index.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_multi_index.py) | 7 | 稀疏向量、BM25、标量索引；其中 5 条依赖内部模块 |
| [test_range_search.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_range_search.py) | 11 | 0016 B 在固定外部原始套件实跑，5 条待复用；6 条依赖内部模块 |
| [test_server_restart.py](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_server_restart.py) | 8 | 重启后读回数据，M1 只在内存中；其中 7 条依赖内部模块（[L24](https://github.com/milvus-io/milvus-lite/blob/v3.2.1/tests/adapter/test_server_restart.py#L24) 顶层导入） |

## tests/compatibility

[tests/compatibility/](https://github.com/milvus-io/milvus-lite/tree/v3.2.1/tests/compatibility) 的 9 个文件共 239 条用例，M1 不复用：它们以范围外的功能为主（过滤、分区与 partition key、多数据库、function、delete / upsert），而且每个文件各自用 `start_server_in_thread` 定义起服务的 fixture。某个文件涉及的功能全部进入范围后，替换 fixture 整文件运行（0004「验收」第 2 项）。

## CLI 入口用例（独立来源）

[test_cli_entrypoints.py](../test_cli_entrypoints.py) 另原样复制参照 tests/test_dump_tool.py 的 test_main_dispatches_server 和 test_package_module_main_delegates_to_cmdline，共 2 条。它们不属于 tests/adapter 的 637 条统计，起服务/信号与参数的真实验证见 test_cli.py 和差分的 test_cli_server.py；dump 尚未复用。

0014 切片 B 原样恢复 8 个函数、参数化 23 条：CRUD LIKE、动态路径/数组及零命中投影、VARCHAR 主键 LIKE、JSON 路径、VARCHAR in/not in。nullable、分区、ARRAY 字段、算术等其他阻断仍保留，不因新增过滤能力扩大复制范围。

0015 B 的固定外部样本新增通过 42 条 ARRAY 相关用例；本表的两个 ARRAY 文件尚未复制，不计入 180 个已复制函数，也不加入默认套件。动态数组过滤的历史范围外条目改为待复用。当前 ARRAY 支持及限定边界见 [B 验收](../../../docs/testing/0015-array-acceptance.md)，历史分节中的 nullable / ARRAY 阻断说明不代表当前缺失。

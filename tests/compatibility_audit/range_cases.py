"""0016 B 的固定请求矩阵；参照真值和差分共用输入，断言由调用方分别执行。"""
import json
import struct

from pymilvus.grpc_gen import common_pb2, milvus_pb2


def request(metric, extra=(), expr='', topk=4, offset=0, queries=None, collection='c'):
    if queries is None:
        queries = [[0., 0.], [1., 0.]] if metric == 'L2' else [[1., 0.], [-1., 0.]]
    ph = common_pb2.PlaceholderGroup()
    v = ph.placeholders.add(tag='$0', type=common_pb2.FloatVector)
    v.values.extend(struct.pack('<'+'f'*len(q), *q) for q in queries)
    r = milvus_pb2.SearchRequest(collection_name=collection, nq=len(queries),
        dsl=expr, dsl_type=common_pb2.BoolExprV1, placeholder_group=ph.SerializeToString(),
        output_fields=['n', 'a', 'd'])
    for key, value in [('anns_field', 'vector'), ('topk', str(topk)),
                       ('metric_type', metric), ('params', '{}'), ('offset', str(offset)), *extra]:
        r.search_params.add(key=key, value=value)
    return r


def matrix(metric):
    yield 'ordinary', request(metric)
    for name, extra in [
        ('inner', [('params', '{"radius":10,"range_filter":1}')]),
        ('outer', [('radius', '10'), ('range_filter', '1')]),
        ('fallback', [('radius','10'), ('params','{"radius":null,"range_filter":1}')]),
        ('precedence', [('radius','999'), ('params','{"radius":10}')]),
        ('params-list', [('params','[]'), ('radius','10')]),
        ('null-absent', [('radius','null'), ('range_filter','null')]),
        ('bool', [('radius','true'), ('range_filter','false')]),
        ('duplicate', [('radius','1'), ('radius','10')]),
        ('round', [('radius','1.25000002'), ('round_decimal','0')]),
        ('unused-overflow', [('params','{"radius":10,"x":1e999}')]),
    ]:
        yield name, request(metric, extra)
    for key in ['radius', 'range_filter']:
        for value in ['-1', '0', '0.25', '1', '1.25', '1.25000002', '1.25000005',
                      '10', '10000', '1e999', '-1e999', '"NaN"', '"Infinity"', '"-Infinity"']:
            yield key+'/'+value, request(metric, [(key,value)])
    for radius, bound in [(1,1),(10,1),(-1,10)]:
        yield f'both/{radius}/{bound}', request(metric, [('radius',str(radius)), ('range_filter',str(bound))])
    for k, off in [(1,0),(2,1),(0,1),(-1,2),(3,-1),(0,0),(-2,0),(1,1000),(2,-5)]:
        yield f'page/{k}/{off}', request(metric, [('radius','10000')], topk=k, offset=off)
    for k, off in [(-2,1),(0,0)]:
        yield f'empty-window/{k}/{off}', request(metric, [('radius','10000')], topk=k, offset=off, expr='d > "x"')
    for expr in ['n is null', 'array_length(a) == 1', 'n is null or d == 3', 'id in [2,7,9]']:
        yield 'filter/'+expr, request(metric, [('radius','10000')], expr=expr)
    for label, extra, expr in [
        ('radius-list',[('radius','[]'),('range_filter','"bad"')],''),
        ('radius-dict',[('radius','{}')],''),
        ('range-text',[('radius','10'),('range_filter','"bad"')],''),
        ('offset',[('radius','10'),('offset','"bad"')],''),
        ('radius-before-offset',[('radius','"bad"'),('offset','"bad"')],''),
        ('huge-integer',[('radius','1'+'0'*400)],''),
        ('group-size',[('radius','10'),('group_size','"bad"')],''),
        ('round',[('radius','10'),('round_decimal','"bad"')],''),
        ('filter',[('radius','10')],'id >'),
    ]:
        yield 'error/'+label, request(metric, extra, expr=expr)
    if metric == 'L2':
        for k, off in [(1,0),(2,0),(1,1),(3,-1)]:
            yield f'window/{k}/{off}', request('L2', [('range_filter','36'), ('radius','10000')],
                                                topk=k, offset=off, queries=[[0.,0.]])


def outcome(response):
    r = response.results
    return {'code': response.status.code, 'error_code': response.status.error_code,
            'reason': response.status.reason, 'ids': list(r.ids.int_id.data),
            'scores': list(r.scores), 'topks': list(r.topks), 'top_k': r.top_k,
            'nq': r.num_queries, 'output_fields': list(r.output_fields),
            'fields': [f.SerializeToString().hex() for f in r.fields_data]}

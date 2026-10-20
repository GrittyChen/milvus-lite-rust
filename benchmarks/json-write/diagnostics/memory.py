from pathlib import Path
import argparse,hashlib,json,os,resource,subprocess,sys,time

def memory():
 return {'rss_bytes':int(subprocess.check_output(['ps','-o','rss=','-p',str(os.getpid())],text=True))*1024,'peak_bytes':resource.getrusage(resource.RUSAGE_SELF).ru_maxrss}

def worker(database):
 from milvus_lite.server_manager import ServerManager
 m=ServerManager();uri=m.start_and_get_uri(database)
 if not uri:raise RuntimeError('server failed')
 print('DIAG '+json.dumps({'uri':uri,'pid':os.getpid(),'memory':memory()}),flush=True)
 for line in sys.stdin:
  cmd=line.strip()
  if cmd=='snapshot':print('DIAG '+json.dumps(memory()),flush=True)
  elif cmd=='stop':
   m.release_all();print('DIAG '+json.dumps({'stopped':True}),flush=True);return
  else:raise RuntimeError('unknown command')
 raise RuntimeError('controller EOF before stop')

def main():
 if len(sys.argv)>1 and sys.argv[1]=='worker':
  if sys.platform != 'darwin':raise RuntimeError('资源口径只验证了 macOS')
  return worker(sys.argv[2])
 p=argparse.ArgumentParser();p.add_argument('--database',type=Path,required=True);p.add_argument('--mode',choices=['default','multi'],required=True);a=p.parse_args()
 repo=Path(__file__).resolve().parents[3]
 log=a.database.with_suffix('.server.log')
 outputs=(a.database,log)
 if (a.database.resolve()==log.resolve() or any(
     path.exists() or path.is_symlink() or path.resolve().is_relative_to(repo)
     or not path.parent.is_dir() for path in outputs)):
  raise ValueError('database 及 server log 必须是仓库外互不相同的新路径，父目录必须存在')
 if sys.platform != 'darwin':raise RuntimeError('资源口径只验证了 macOS')
 with log.open('x') as err:
  run_probe(a,err)

def run_probe(a,err):
 import grpc
 from pymilvus import MilvusClient,DataType
 from pymilvus.grpc_gen import schema_pb2 as pb,milvus_pb2 as rpc
 proc=subprocess.Popen([sys.executable,'-B',__file__,'worker',str(a.database)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=err,text=True)
 def receive():
  while True:
   line=proc.stdout.readline()
   if not line:raise RuntimeError('worker EOF, code='+str(proc.poll()))
   if line.startswith('DIAG '):return json.loads(line[5:])
 def server():proc.stdin.write('snapshot\n');proc.stdin.flush();return receive()
 initial=receive();c=MilvusClient(initial['uri'],dedicated=True);ch=grpc.insecure_channel(initial['uri'].removeprefix('http://'),options=[('grpc.max_send_message_length',-1),('grpc.max_receive_message_length',-1)])
 stages=[{'stage':'ready','server':initial['memory'],'client':memory()}];latency={}
 def snapshot(name):stages.append({'stage':name,'server':server(),'client':memory()})
 try:
  s=c.create_schema(enable_dynamic_field=False);s.add_field('id',DataType.INT64,is_primary=True);s.add_field('k',DataType.INT64,is_partition_key=a.mode!='default');s.add_field('vector',DataType.FLOAT_VECTOR,dim=2);s.add_field('value',DataType.JSON)
  idx=c.prepare_index_params();idx.add_index('vector',index_type='FLAT',metric_type='L2');c.create_collection('c',schema=s,index_params=idx)
  n=5000;value={'body':'x'*2048,'values':list(range(32))};key=lambda i:i%16
  fields=[pb.FieldData(field_name='id',type=5,scalars=pb.ScalarField(long_data=pb.LongArray(data=range(n)))),pb.FieldData(field_name='k',type=5,scalars=pb.ScalarField(long_data=pb.LongArray(data=[key(i) for i in range(n)]))),pb.FieldData(field_name='vector',type=101,vectors=pb.VectorField(dim=2,float_vector=pb.FloatArray(data=[1.,0.]*n))),pb.FieldData(field_name='value',type=23,scalars=pb.ScalarField(json_data=pb.JSONArray(data=[json.dumps(value).encode()]*n)))]
  expected=[{'id':i,'k':key(i),'value':value} for i in range(n)]
  requests={method:getattr(rpc,method+'Request')(collection_name='c',num_rows=n,fields_data=fields).SerializeToString() for method in ['Insert','Upsert']}
  snapshot('prepared')
  for method in ['Insert','Upsert']:
   call=ch.unary_unary('/milvus.proto.milvus.MilvusService/'+method,request_serializer=None,response_deserializer=rpc.MutationResult.FromString);times=[]
   for i in range(6):
    t=time.perf_counter();v=call(requests[method]);times.append((time.perf_counter()-t)*1000)
    assert (v.status.code,v.status.error_code,v.status.reason)==(0,0,'')
    assert list(v.IDs.int_id.data)==list(range(n)) and list(v.succ_index)==list(range(n)) and not v.err_index
    assert (v.insert_cnt if method=='Insert' else v.upsert_cnt)==n
    snapshot(method.lower()+'-'+str(i))
   latency[method]=times
   actual=c.query('c',filter='',output_fields=['id','k','value'],limit=n)
   assert json.dumps(sorted(actual,key=lambda x:x['id']),sort_keys=True)==json.dumps(expected,sort_keys=True)
   snapshot(method.lower()+'-verified')
  c.flush('c');assert c.query('c',output_fields=['count(*)'])==[{'count(*)':n}];snapshot('flushed')
 finally:
  c.close();ch.close();proc.stdin.write('stop\n');proc.stdin.flush();assert receive()=={'stopped':True};assert proc.wait(timeout=60)==0
 print(json.dumps({'passed':True,'mode':a.mode,'rows':5000,'stages':stages,'latency_ms':latency,'probe_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),'scope':'separate worker server and client; current RSS and process high-water; snapshots outside Insert/Upsert; no allocator attribution'}))
if __name__=='__main__':main()

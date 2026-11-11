(() => {
  const sdk=globalThis.__ownedSdk, base=sdk.TAbstractFile, rows=[], vault={ownedVault:true};
  const units=s=>Array.from({length:s.length},(_,i)=>s.charCodeAt(i));
  const describe=v=>v===vault?{identity:'vault'}:v===null?{type:'null'}:typeof v==='undefined'?{type:'undefined'}:typeof v==='string'?{type:'string',units:units(v)}:typeof v==='number'?{type:'number',value:String(v)}:typeof v==='boolean'?{type:'boolean',value:v}:Array.isArray(v)?{type:'Array',length:v.length}:{type:typeof v};
  const own=o=>o&&typeof o==='object'?Object.entries(Object.getOwnPropertyDescriptors(o)).map(([key,d])=>({key,enumerable:d.enumerable,configurable:d.configurable,...(Object.hasOwn(d,'value')?{writable:d.writable,value:describe(d.value)}:{get:typeof d.get,set:typeof d.set})})):describe(o);
  function run(id,klass,prepare,operation='construct') {
    const log=[],restore=prepare(log);let receiver={},result;
    try {
      if(operation==='construct'){result=Reflect.construct(sdk[klass],[vault,'Input.MD']);receiver=result;}
      else result=Reflect.apply(sdk.TFile.prototype.setPath,receiver,['Input.MD']);
      rows.push({id,result:result===receiver?{identity:'receiver'}:describe(result),own:own(receiver),log});
    }catch(e){rows.push({id,error:{name:e.name,message:e.message},own:own(receiver),log});}
    finally{restore();}
  }
  const replaceCall=fn=>{const d=Object.getOwnPropertyDescriptor(base,'call');Object.defineProperty(base,'call',{value:fn,writable:true,configurable:true});return()=>{if(d)Object.defineProperty(base,'call',d);else delete base.call;};};
  for(const klass of ['TFile','TFolder']) {
    run(klass+'-base-call-object',klass,log=>replaceCall(function(receiver,v,p){log.push({base_call:true,vault_same:v===vault,path:describe(p),receiver_same:receiver===this});return {replacement:true};}));
    run(klass+'-base-call-falsy',klass,log=>replaceCall(function(){log.push('base_call');return 0;}));
    run(klass+'-base-call-primitive',klass,log=>replaceCall(function(){log.push('base_call');return 'YES';}));
    run(klass+'-base-call-noncallable',klass,()=>replaceCall(3));
    run(klass+'-base-call-getter',klass,log=>{const old=Object.getOwnPropertyDescriptor(base,'call');Object.defineProperty(base,'call',{configurable:true,get(){log.push('get:base.call');return Function.prototype.call;}});return()=>{if(old)Object.defineProperty(base,'call',old);else delete base.call;};});
  }
  run('setPath-base-method-replaced','TFile',log=>{const proto=base.prototype,old=proto.setPath;proto.setPath=function(p){log.push({base_setPath:describe(p)});this.path='OVERRIDE';this.name='CUSTOM.Z';};return()=>proto.setPath=old;},'setPath');
  run('setPath-base-method-call-replaced','TFile',log=>{const method=base.prototype.setPath,old=Object.getOwnPropertyDescriptor(method,'call');Object.defineProperty(method,'call',{configurable:true,value:function(receiver,p){log.push({method_call:true,path:describe(p),function_same:this===method});receiver.path='OVERRIDE';receiver.name='CUSTOM.Z';return {ignored:true};}});return()=>{if(old)Object.defineProperty(method,'call',old);else delete method.call;};},'setPath');
  run('setPath-base-prototype-replaced','TFile',log=>{const old=base.prototype;base.prototype={setPath(p){log.push({new_prototype:describe(p)});this.path='OVERRIDE';this.name='CUSTOM.Z';}};return()=>base.prototype=old;},'setPath');
  return JSON.stringify(rows);
})()

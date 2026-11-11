(() => {
  const method = app.vault.adapter.getBasePath, marker = {owned:true};
  const units = s => Array.from({length:s.length}, (_,i) => s.charCodeAt(i));
  const value = v => v===marker?{identity:'marker'}:v===undefined?{type:'undefined'}:typeof v==='string'?{type:'string',units:units(v)}:{type:typeof v};
  const cases=[];
  for(const [id,input] of [['text',{basePath:'OWNED'}],['missing',{}],['null',null],['undefined',undefined],['number',3],['object',{basePath:marker}],['lone',{basePath:String.fromCharCode(0xd800)}]]) {
    try { cases.push({id,result:value(Reflect.apply(method,input,[]))}); }
    catch(e) { cases.push({id,error:{name:e.name,message:e.message}}); }
  }
  const log=[];
  try { cases.push({id:'getter',result:value(Reflect.apply(method,{get basePath(){log.push('basePath');return marker;}},[])),log}); }
  catch(e) { cases.push({id:'getter',error:{name:e.name,message:e.message},log}); }
  const fields=[[app,'vault'],[app.vault,'adapter'],[app.vault.adapter,'basePath']].map(([object,key])=>{
    const descriptor=Object.getOwnPropertyDescriptor(object,key);
    return {key,enumerable:descriptor.enumerable,configurable:descriptor.configurable,writable:descriptor.writable};
  });
  return JSON.stringify({cases,method:{name:method.name,length:method.length,own_keys:Reflect.ownKeys(method).filter(key=>typeof key==='string')},fields});
})()

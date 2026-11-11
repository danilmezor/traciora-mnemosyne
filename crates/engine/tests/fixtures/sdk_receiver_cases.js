JSON.stringify((() => {
  const units = value => Array.from({length:value.length}, (_, i) => value.charCodeAt(i));
  const describe = (v, input) => v === null ? {type:"null"} : typeof v === "undefined" ? {type:"undefined"} : typeof v === "string" ? {type:"string", units:units(v)} : typeof v === "number" ? {type:"number", value:String(v)} : typeof v === "boolean" ? {type:"boolean", value:v} : {type:typeof v, same_input:v === input, array:Array.isArray(v)};
  const rows = [];
  function run(id, klass, method, fn, receiver, input) {
    const log = [];
    const actual = typeof receiver === "function" ? receiver(log) : receiver;
    try { rows.push({id, class:klass, method, result:describe(fn.call(actual), input), log}); }
    catch(e) { rows.push({id, class:klass, method, error:{name:e.name, message:e.message}, log}); }
  }
  const getter = (log, key, value, fail=false) => ({get(){log.push(key); if(fail) throw Error("owned getter fault"); return value;}, enumerable:true, configurable:true});
  run("short-md","TFile","getShortName",ownedShort,log => Object.defineProperties({}, {extension:getter(log,"extension","md"), basename:getter(log,"basename","BASE"), name:getter(log,"name","unused")}));
  run("short-uppercase","TFile","getShortName",ownedShort,log => Object.defineProperties({}, {extension:getter(log,"extension","MD"), basename:getter(log,"basename","unused"), name:getter(log,"name","Name.MD")}));
  run("short-missing","TFile","getShortName",ownedShort,{});
  const identity = {owned:true};
  run("short-object","TFile","getShortName",ownedShort,{extension:"md", basename:identity},identity);
  run("short-null","TFile","getShortName",ownedShort,null);
  run("short-undefined","TFile","getShortName",ownedShort,undefined);
  run("short-extension-throws","TFile","getShortName",ownedShort,log => Object.defineProperty({},"extension",getter(log,"extension",null,true)));
  run("toString-text","TFile","toString",ownedString,log => Object.defineProperty({},"path",getter(log,"path","Folder/A.md")));
  run("toString-lone","TFile","toString",ownedString,{path:String.fromCharCode(0xd800)});
  run("toString-missing","TFile","toString",ownedString,{});
  run("toString-object","TFile","toString",ownedString,{path:identity},identity);
  run("toString-number","TFile","toString",ownedString,{path:3});
  run("toString-null","TFile","toString",ownedString,null);
  run("root-borrowed","TFolder","isRoot",ownedRoot,log => Object.defineProperty({},"path",getter(log,"path","/")));
  run("root-boxed","TFolder","isRoot",ownedRoot,{path:new String("/")});
  run("root-missing","TFolder","isRoot",ownedRoot,{});
  return rows;
})())

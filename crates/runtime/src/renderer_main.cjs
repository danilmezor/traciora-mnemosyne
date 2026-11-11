'use strict';
// Electron lifecycle plumbing only. Rust owns profile, socket, dispatch and eval.
const { app, BrowserWindow, protocol, net } = require('electron');
const fs = require('fs');
const path = require('path');
const remote = require('./node_modules/@electron/remote/main');
const { pathToFileURL } = require('url');
protocol.registerSchemesAsPrivileged([{ scheme: 'app', privileges: {
  standard: true, secure: true, supportFetchAPI: true,
  stream: true, codeCache: true, corsEnabled: true
} }]);
app.setPath('userData', path.join(__dirname, 'user-data'));
remote.initialize();
let window;
app.whenReady().then(async () => {
  protocol.handle('app', request => {
    const url = new URL(request.url);
    if (url.hostname !== 'mnemosyne' || url.pathname !== '/runtime.html')
      return new Response('Owned runtime page missing', { status: 404 });
    return net.fetch(pathToFileURL(path.join(__dirname, 'runtime.html')).href);
  });
  window = new BrowserWindow({ show: false, webPreferences: {
    nodeIntegration: true, contextIsolation: false,
    nodeIntegrationInWorker: true, webviewTag: true
  } });
  remote.enable(window.webContents);
  window.webContents.on('render-process-gone', (_event, details) => {
    fs.writeFileSync(path.join(__dirname, 'renderer-stop.txt'), `${details.reason} ${details.exitCode}`);
    app.exit(details.exitCode);
  });
  window.webContents.on('did-finish-load', () => {
    const pid = window.webContents.getOSProcessId();
    fs.writeFileSync(path.join(__dirname, 'renderer-ready.txt'), `${process.pid} ${pid}`);
    window.webContents.executeJavaScript(fs.readFileSync(path.join(__dirname, 'renderer-bootstrap.js'), 'utf8'))
      .catch(error => { console.error(error); app.exit(1); });
  });
  await window.loadURL('app://mnemosyne/runtime.html');
}).catch(error => { console.error(error); app.exit(1); });
app.on('window-all-closed', () => app.exit(0));

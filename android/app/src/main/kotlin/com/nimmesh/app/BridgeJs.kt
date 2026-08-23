package com.nimmesh.app

/**
 * The `window.nimmesh` shim, injected before any page script runs.
 *
 * This is the Android half of a deliberately identical contract. webui/ is the SAME
 * directory the iOS app bundles and contains zero platform code: it calls
 * `window.nimmesh.<method>()` and awaits a Promise, and knows nothing about which OS
 * answered. Only the transport line differs here, so the web layer stays one codebase.
 *
 * The wire format is the iOS one verbatim: `{id, method, args}` goes out, and the answer
 * comes back as `window.__nimmeshResolve(id, ok, payload)`. On iOS that ride is
 * `webkit.messageHandlers.nimmesh.postMessage`; here it is an `@JavascriptInterface`
 * method, which takes a STRING rather than a structured object, so the payload is
 * JSON-encoded on the way out and decoded on the way in.
 *
 * The method list below is generated from WebHostView.swift's `jsShim` and is kept
 * honest by BridgeMethodParityTest, which fails if the two ever drift apart. Add a
 * method to one platform and the other's build tells you.
 */
object BridgeJs {

    /** Every method name the page can call. The bridge rejects anything not in here. */
    val METHODS: Set<String> = setOf(
        "version",
        "meshStatus",
        "reachability",
        "backupUrgency",
        "walletAddress",
        "walletExists",
        "createWallet",
        "importWallet",
        "recoveryPhrase",
        "walletStatus",
        "resolveRecovered",
        "deleteWallet",
        "getLang",
        "setLang",
        "scanQr",
        "share",
        "meshDebug",
        "keepalive",
        "getRespondRole",
        "setRespondRole",
        "authenticate",
        "backupCodes",
        "importBackupCodes",
        "getBackedUp",
        "setBackedUp",
        "headHeight",
        "walletBalance",
        "walletHistory",
        "sendTransaction",
        "meshSendInfo",
        "meshSendTransaction",
        "meshPaymentStatus",
        "prices",
        "market",
        "meshQueryBalance",
        "meshCachedBalance",
        "meshQueryHistory",
        "meshCachedHistory",
        "swapMeshStart",
        "swapMeshStatus",
        "swapMeshStop",
        "swapMeshRefund",
        "mainnetSwapArmed",
        "swapEvmAddresses",
        "usdcBalances",
        "usdcHistory",
        "sendUsdc",
        "sendChat",
        "chatMessages",
        "bitchatStatus",
        "bitchatSetEnabled",
        "cashlinkCreate",
        "cashlinkList",
        "cashlinkStatus",
        "cashlinkPeek",
        "cashlinkClaim"

    )

    val SHIM: String = """
    (function () {
      if (!(window.__nimmeshNative && window.__nimmeshNative.postMessage)) return;
      var pending = {}, seq = 0;
      function call(method, args) {
        return new Promise(function (resolve, reject) {
          var id = ++seq;
          pending[id] = { resolve: resolve, reject: reject };
          try { window.__nimmeshNative.postMessage(JSON.stringify({ id: id, method: method, args: args === undefined ? null : args })); }
          catch (e) { delete pending[id]; reject(e); }
        });
      }
      window.__nimmeshResolve = function (id, ok, payload) {
        var p = pending[id]; if (!p) return; delete pending[id];
        if (ok) p.resolve(payload); else p.reject(new Error(String(payload)));
      };
      window.nimmesh = {
        call: call,
        version: function () { return call('version'); },
        meshStatus: function () { return call('meshStatus'); },
        reachability: function () { return call('reachability'); },
        backupUrgency: function (s) { return call('backupUrgency', s || {}); },
        walletAddress: function () { return call('walletAddress'); },
        walletExists: function () { return call('walletExists'); },
        createWallet: function () { return call('createWallet'); },
        importWallet: function (m) { return call('importWallet', { mnemonic: m }); },
        recoveryPhrase: function () { return call('recoveryPhrase'); },
        walletStatus: function () { return call('walletStatus'); },
        resolveRecovered: function (keep) { return call('resolveRecovered', { keep: !!keep }); },
        deleteWallet: function () { return call('deleteWallet'); },
        getLang: function () { return call('getLang'); },
        setLang: function (l) { return call('setLang', { lang: l }); },
        scanQr: function () { return call('scanQr'); },
        share: function (text, url) { return call('share', { text: text, url: url }); },
        meshDebug: function () { return call('meshDebug'); },
        keepalive: function () { return call('keepalive'); },
        getRespondRole: function () { return call('getRespondRole'); },
        setRespondRole: function (a) { return call('setRespondRole', a); },
        authenticate: function () { return call('authenticate'); },
        backupCodes: function () { return call('backupCodes'); },
        importBackupCodes: function (a, b) { return call('importBackupCodes', { code1: a, code2: b }); },
        getBackedUp: function () { return call('getBackedUp'); },
        setBackedUp: function (v) { return call('setBackedUp', { backedUp: !!v }); },
        headHeight: function () { return call('headHeight'); },
        walletBalance: function () { return call('walletBalance'); },
        walletHistory: function () { return call('walletHistory'); },
        sendTransaction: function (a) { return call('sendTransaction', a || {}); },
        meshSendInfo: function () { return call('meshSendInfo'); },
        meshSendTransaction: function (a) { return call('meshSendTransaction', a || {}); },
        meshPaymentStatus: function (t) { return call('meshPaymentStatus', { meshTxId: t }); },
        prices: function (c) { return call('prices', { currency: c }); },
        market: function (coin, c) { return call('market', { coin: coin, currency: c }); },
        meshQueryBalance: function () { return call('meshQueryBalance'); },
        meshCachedBalance: function () { return call('meshCachedBalance'); },
        meshQueryHistory: function () { return call('meshQueryHistory'); },
        meshCachedHistory: function () { return call('meshCachedHistory'); },
        swapMeshStart: function (a) { return call('swapMeshStart', a || {}); },
        swapMeshStatus: function () { return call('swapMeshStatus'); },
        swapMeshStop: function () { return call('swapMeshStop'); },
        swapMeshRefund: function () { return call('swapMeshRefund'); },
        mainnetSwapArmed: function () { return call('mainnetSwapArmed'); },
        swapEvmAddresses: function () { return call('swapEvmAddresses'); },
        usdcBalances: function (a) { return call('usdcBalances', a || {}); },
        usdcHistory: function (a) { return call('usdcHistory', a || {}); },
        sendUsdc: function (a) { return call('sendUsdc', a || {}); },
        sendChat: function (nick, text) { return call('sendChat', { nickname: nick, text: text }); },
        chatMessages: function () { return call('chatMessages'); },
        bitchatStatus: function () { return call('bitchatStatus'); },
        bitchatSetEnabled: function (a) { return call('bitchatSetEnabled', a || {}); },
        cashlinkCreate: function (a) { return call('cashlinkCreate', a || {}); },
        cashlinkList: function () { return call('cashlinkList'); },
        cashlinkStatus: function (addr) { return call('cashlinkStatus', { address: addr }); },
        cashlinkPeek: function (a) { return call('cashlinkPeek', a || {}); },
        cashlinkClaim: function (a) { return call('cashlinkClaim', a || {}); }
      };
    })();
    """
}

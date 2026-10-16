import assert from "node:assert/strict";
import test from "node:test";
import { connectionSucceeded, explainFailure, latestModelEvents, eventPurpose, eventHasProblem } from "../src/diagnosis.ts";
import { normalizeView } from "../src/state.ts";
import type { ObservationEvent, ProviderConnectionReport } from "@codetas/core";
const probe = (status: number, authenticated: boolean): ProviderConnectionReport => ({providerId:"p",reachable:true,authenticated,status,latencyMs:1,modelCount:0,message:"test"});
const event = (at: number, status: number, extra = {}): ObservationEvent => ({exposedModel:"p/m",timestampMs:at,ledgerSequence:at,statusCode:status,sendCount:1,totalTokens:0,recoveryKinds:[],shadow:false,attemptOnly:false,...extra}) as ObservationEvent;
test("authentication and HTTP errors never count as a successful connection", () => {
 assert.equal(connectionSucceeded(probe(401,false)),false);
 assert.equal(connectionSucceeded(probe(403,false)),false);
 assert.equal(connectionSucceeded(probe(429,true)),false);
 assert.equal(connectionSucceeded(probe(200,true)),true);
 assert.equal(connectionSucceeded(probe(200,false)),false);
});
test("quota errors have actionable advice without assuming the user consumed the quota", () => {
 const p=explainFailure(429,"Individual quota reached");assert.match(p.cause,/限りません/);assert.match(p.action,/リセット/);
 assert.match(explainFailure(401,"").action,/再ログイン/);
});
test("local completion cannot hide the original upstream quota error", () => {
 const e=event(2,200,{upstreamError:{statusCode:429,errorMessage:"quota"},recoveryKinds:["local-compaction-offline"]});
 assert.equal(eventHasProblem(e),true);assert.equal(eventPurpose(e),"圧縮・復旧");
 assert.match(eventPurpose(event(3,200,{sendCount:0,recoveryKinds:["local-compaction-quota"]})),/外部送信なし/);
});
test("newer success replaces older failures, but attempts and shadow calls do not", () => {
 const rows=latestModelEvents([event(1,429),event(2,200),event(3,500,{attemptOnly:true}),event(4,500,{shadow:true})]);
 assert.equal(rows.length,1);assert.equal(rows[0]!.statusCode,200);
});
test("legacy destinations resolve into exactly four views", () => {
 assert.equal(normalizeView("overview"),"status");assert.equal(normalizeView("agents"),"models");
 assert.equal(normalizeView("maintenance"),"settings");assert.equal(normalizeView("clients"),"connections");
 assert.equal(normalizeView("unknown"),"connections");
});

test("quota reset estimate uses the event time and rejects malformed durations", async () => {
 const { quotaResetAt } = await import("../src/diagnosis.ts");
 assert.equal(quotaResetAt(event(1000,200,{upstreamError:{statusCode:429,errorMessage:"Individual quota reached. Resets in 2h3m4s."}})),7385000);
 assert.equal(quotaResetAt(event(1000,429,{upstreamError:{statusCode:429,errorMessage:"Resets in invalid."}})),null);
 assert.equal(quotaResetAt(event(1000,200)),null);
});

test("offline compression and local cooldown cannot erase the last upstream failure", () => {
 const quota=event(1,429);
 const offline=event(2,200,{sendCount:0,recoveryKinds:["local-compaction-quota"]});
 const cooling=event(3,503,{sendCount:0,failureCategory:"provider_cooling_down"});
 assert.equal(latestModelEvents([cooling,offline,quota])[0]!.statusCode,429);
 assert.equal(latestModelEvents([quota,offline,event(4,200)])[0]!.statusCode,200);
});

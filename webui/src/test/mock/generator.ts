/**
 * 测试专用样本生成器，只供契约、指标与视图测试使用。
 *
 * 覆盖的场景：
 *   跨天事件 · 多客服协作 · 未回复 · 超时 · 平台发起（首响恒 0 秒）·
 *   归不上去的 __untyped__ · 抽取失败的群日（事件级 NULL、agent 行整行缺失）
 *
 * 种子固定，**随机数消耗顺序与引擎无关**（洗牌用 Fisher-Yates，不用随机比较器排序），
 * 所以任何浏览器、任何 Node 版本跑出来的是同一份数据。
 */

import { formatDateTime, parseDateTime } from "@/lib/format";
import { UNTYPED } from "@/domain/definitions";
import { workSecsBetween } from "@/domain/worktime";
import type {
  AgentDailyRow,
  EventRow,
  FailureRow,
  GroupDailyRow,
  MessageRow,
  Meta,
} from "@/domain/schemas";
import { MOCK_TAXONOMY } from "./taxonomy";

export interface MockDataset {
  meta: Meta;
  events: EventRow[];
  groupDaily: GroupDailyRow[];
  agentDaily: AgentDailyRow[];
  failures: FailureRow[];
  messages: Map<number, MessageRow[]>;
}

const CORPID = "ww4f9a1c77e0b3d5a2";
const TAXONOMY_VERSION = "v1";
const DAYS = [
  "2026-08-25",
  "2026-08-26",
  "2026-08-27",
  "2026-08-28",
  "2026-08-29",
  "2026-08-30",
  "2026-08-31",
];
/** roomIndex|date：这三格抽取失败，事件级指标是 NULL，agent 表整行缺失 */
const FAILED_CELLS = new Set(["3|2026-08-27", "3|2026-08-28", "9|2026-08-26"]);

const ROOM_ALIAS = [
  "华东-家电安装A组",
  "华东-家电安装B组",
  "华南-灯具安装群",
  "华南-售后返工群",
  "西南-送装一体群",
  "华北-空调维修群",
  "华北-家政保洁群",
  "华中-拆旧协作群",
  "东北-大件配送群",
  "西北-维修派单群",
  "江浙-旗舰店专群",
  "闽粤-新店筹备群",
  "川渝-加急处理群",
  "京津-VIP商户群",
];
const AGENT_ALIAS = ["林可", "周叙", "唐棠", "邵珩", "岑澜", "阮野", "毕遥", "柯茉"];
const ROOM_HEAT = [1.6, 1.4, 1.25, 1.1, 1.05, 1, 0.95, 0.9, 0.85, 0.8, 0.75, 0.7, 0.6, 0.5];

const L1_WEIGHT: Record<string, number> = {
  履约催促: 26,
  订单变更: 17,
  服务售后: 14,
  信息异常: 12,
  费用结算: 11,
  师傅调度: 9,
  业务查询: 7,
  损坏赔付: 3,
  无明确诉求: 1,
};

/** 摘要契约：中文一句话、不超过 100 字、**不含订单号等 ID**（落库前由抽取校验器拦截）。 */
const SUMMARY: Record<string, string[]> = {
  履约催促: [
    "客户已在家等候，要求尽快安排师傅上门",
    "订单挂出两小时仍无人接单，商家催促处理",
    "已过约定时段师傅仍未出发，要求今日内到场",
    "客户第三次来电催问上门时间，请尽快回复",
    "要求平台加急处理这一单，客户情绪较激动",
  ],
  师傅调度: [
    "原师傅临时有事无法上门，要求改派他人",
    "商家点名要求由熟悉该机型的师傅承接",
    "要求转告师傅带齐梯子和电钻再出发",
    "客户反映师傅沟通态度不佳，要求更换",
    "现场需两人配合施工，请求协调增派人手",
  ],
  订单变更: [
    "客户临时增加两处吸顶灯安装点位",
    "客户取消其中一项拆旧服务，其余照常",
    "上门时间由本周五调整到下周一上午",
    "联系地址填错，需更正为同小区另一栋",
    "客户装修尚未完工，要求先挂起订单",
  ],
  业务查询: [
    "商家询问该单目前进行到了哪一步",
    "希望确认接单师傅的联系方式",
    "询问该地址是否在平台服务覆盖范围内",
    "确认这种超高吊顶的灯具能否安装",
    "查询订单当前状态是否已标记完成",
  ],
  费用结算: [
    "商家询问拆旧服务的收费标准",
    "师傅到场客户不在家，申请收取空跑费",
    "争议这笔远程费应由商家还是客户承担",
    "客户反映实际收费高于下单时的报价",
    "服务最终未完成，商家要求退回已付费用",
  ],
  信息异常: [
    "师傅端看不到这个订单，无法接单",
    "系统显示已完成但客户称师傅从未上门",
    "订单显示两件，实际现场有六件待安装",
    "客户预留电话空号，师傅始终联系不上",
    "同一笔业务被重复下了两个订单",
  ],
  服务售后: [
    "安装完成后灯具晃动并有明显异响",
    "五件商品只安装了三件，师傅提前离场",
    "要求师傅再上门一次做角度调整",
    "师傅称已完工但客户否认，双方说法不一",
    "现场无预留电源，师傅到场后无法施工",
  ],
  损坏赔付: [
    "安装过程中碰坏客户墙面，需处理",
    "正在核实灯具损坏是否由师傅操作造成",
    "责任已明确，商家与平台商谈赔偿金额",
  ],
  无明确诉求: ["仅发来订单号和客户手机号，未说明诉求", "仅上传了现场照片，没有文字说明"],
  未归类: ["商家发来一段情况说明，诉求需要人工判断"],
};
const REPLY = [
  "稍等，我看一下。",
  "好的，已收到，马上安排。",
  "已催促师傅，稍后给您回复。",
  "已反馈给调度，请稍等。",
  "已加单，麻烦确认一下。",
  "这边核实一下再回复您。",
  "已安排师傅今天下午过去。",
];
const FOLLOW_EXT = [
  "麻烦快点，客户一直在催。",
  "还有个情况补充一下。",
  "客户说还是没接到电话。",
  "好的，那就麻烦你们了。",
  "这个能今天解决吗？",
];
const FOLLOW_INT = [
  "已联系上师傅，预计一小时内出发。",
  "师傅说已经在路上了。",
  "已经改派给另一位师傅了。",
  "核实过了，费用这边可以承担。",
  "已处理完，麻烦跟客户确认一下。",
];

/**
 * 与 `assemble::followup_wait_max_sec` 同一条规则：**首响之后**每次
 * EXTERNAL→INTERNAL 的工作时段间隔取最大，末尾没人接的那一段不计。
 * 返回 0 = 确实没有后续轮次。
 */
function followupWaitMaxSec(msgs: MessageRow[]): number {
  const firstReply = msgs.findIndex((m) => m.sender_role === "INTERNAL");
  if (firstReply < 0) return 0;
  let max = 0;
  let waiting: string | null = null;
  for (const m of msgs.slice(firstReply + 1)) {
    if (m.sender_role === "EXTERNAL") {
      waiting ??= m.at;
    } else if (waiting !== null) {
      max = Math.max(max, workSecsBetween(waiting, m.at));
      waiting = null;
    }
  }
  return max;
}

function makeRng(seed: number): () => number {
  let s = seed | 0;
  return () => {
    s = (s + 0x6d2b79f5) | 0;
    let t = Math.imul(s ^ (s >>> 15), 1 | s);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export function buildMockDataset(seed = 20260829): MockDataset {
  const rnd = makeRng(seed);
  const pick = <T>(a: readonly T[]): T => a[Math.floor(rnd() * a.length)] as T;
  const rint = (a: number, b: number) => a + Math.floor(rnd() * (b - a + 1));
  const hex = (n: number) =>
    Array.from({ length: n }, () => "0123456789abcdef"[Math.floor(rnd() * 16)]).join("");
  /** Fisher-Yates。不用随机比较器排序：那既有偏，消耗的随机数个数还依赖引擎实现。 */
  const shuffle = <T>(arr: readonly T[]): T[] => {
    const a = [...arr];
    for (let i = a.length - 1; i > 0; i--) {
      const j = Math.floor(rnd() * (i + 1));
      [a[i], a[j]] = [a[j] as T, a[i] as T];
    }
    return a;
  };
  const weighted = (w: Record<string, number>): string => {
    const total = Object.values(w).reduce((s, v) => s + v, 0);
    let r = rnd() * total;
    for (const [k, v] of Object.entries(w)) {
      r -= v;
      if (r <= 0) return k;
    }
    return Object.keys(w)[0] as string;
  };
  const replyDelaySec = () => {
    const r = rnd();
    if (r < 0.55) return rint(20, 300); // 五分钟内，「稍等」就是受理应答
    if (r < 0.78) return rint(300, 1800);
    if (r < 0.9) return rint(1800, 7200); // 默认阈值下已超时
    if (r < 0.97) return rint(7200, 54000);
    return rint(54000, 140000); // 跨天才回，最长约 39 小时
  };

  const rooms = ROOM_ALIAS.map((alias, i) => ({
    roomid: `R${hex(16)}`,
    alias,
    heat: ROOM_HEAT[i] ?? 1,
  }));
  const agents = AGENT_ALIAS.map((alias) => ({ agent: hex(16), alias }));
  const askers = Array.from({ length: 15 }, () => hex(16));

  const events: EventRow[] = [];
  const groupDaily: GroupDailyRow[] = [];
  const agentDaily: AgentDailyRow[] = [];
  const failures: FailureRow[] = [];
  const messages = new Map<number, MessageRow[]>();
  let eventId = 0;
  let msgSeq = 0;

  for (let ri = 0; ri < rooms.length; ri++) {
    const room = rooms[ri] as (typeof rooms)[number];
    const crew = shuffle(agents).slice(0, rint(2, 4));
    for (const dt of DAYS) {
      const failed = FAILED_CELLS.has(`${ri}|${dt}`);
      const n = Math.max(0, Math.round(rint(3, 9) * room.heat));
      const dayEvents: EventRow[] = [];

      for (let k = 0; k < n; k++) {
        const t0 = parseDateTime(`${dt} 08:30:00`);
        t0.setTime(t0.getTime() + rint(0, 13 * 3600) * 1000);
        const isPush = rnd() < 0.11;
        const level1 = weighted(L1_WEIGHT);
        const candidates = MOCK_TAXONOMY.filter((t) => t.parent_name === level1);
        const primary = pick(candidates);
        // 一个事件一个类。3% 归不上去 —— `vN` + `__untyped__` 是数据信号，页面要能显示它。
        const type = rnd() < 0.03 ? UNTYPED : primary.type_id;

        let asker: string;
        let askerRole: "EXTERNAL" | "INTERNAL";
        let eventAgents: string[];
        let firstResponder: string | null;
        let replyAt: Date | null;

        if (isPush) {
          asker = pick(crew).agent;
          askerRole = "INTERNAL";
          eventAgents = [asker];
          firstResponder = asker;
          replyAt = new Date(t0); // 平台推送：首响恒 0 秒
        } else {
          asker = pick(askers);
          askerRole = "EXTERNAL";
          if (rnd() < 0.13) {
            eventAgents = [];
            firstResponder = null;
            replyAt = null; // 未回复，agents 实测也是空
          } else {
            const count = rnd() < 0.74 ? 1 : rnd() < 0.85 ? 2 : 3;
            eventAgents = shuffle(crew)
              .slice(0, Math.min(count, crew.length))
              .map((a) => a.agent);
            firstResponder = eventAgents[0] ?? null;
            replyAt = new Date(t0.getTime() + replyDelaySec() * 1000);
          }
        }

        const tailSec = replyAt
          ? rint(120, 9000) + (rnd() < 0.12 ? rint(20 * 3600, 40 * 3600) : 0)
          : rint(60, 3600);
        const lastAt = new Date((replyAt ?? t0).getTime() + tailSec * 1000);
        const l1name = MOCK_TAXONOMY.find((t) => t.type_id === type)?.parent_name ?? "未归类";
        const summary = `${pick(SUMMARY[l1name] ?? SUMMARY["无明确诉求"] ?? [""])}。`;

        const id = ++eventId;
        const msgs: MessageRow[] = [];
        const sourceIds: string[] = [];
        const orderNo = String(rint(20260800000, 20260899999));
        const emit = (at: Date, senderId: string, role: "EXTERNAL" | "INTERNAL", text: string) => {
          const m: MessageRow = {
            msg_id: `m${String(++msgSeq).padStart(7, "0")}`,
            at: formatDateTime(at),
            sender_id: senderId,
            sender_role: role,
            text,
          };
          msgs.push(m);
          sourceIds.push(m.msg_id);
        };

        emit(
          t0,
          asker,
          askerRole,
          `${askerRole === "INTERNAL" ? "【工单推送】" : ""}${summary.replace("。", "")}，订单号 ${orderNo}。`,
        );
        if (replyAt && firstResponder) {
          emit(replyAt, firstResponder, "INTERNAL", pick(REPLY));
          const extra = rint(0, 3);
          for (let j = 0; j < extra; j++) {
            const at = new Date(
              replyAt.getTime() + ((j + 1) * (lastAt.getTime() - replyAt.getTime())) / (extra + 1),
            );
            const fromMerchant = j % 2 === 0 && askerRole === "EXTERNAL";
            const sid = fromMerchant
              ? asker
              : j % 2 === 1 && eventAgents.length > 1
                ? (eventAgents[1] as string)
                : firstResponder;
            emit(
              at,
              sid,
              fromMerchant ? "EXTERNAL" : "INTERNAL",
              fromMerchant ? pick(FOLLOW_EXT) : pick(FOLLOW_INT),
            );
          }
          if (lastAt > replyAt) {
            emit(
              lastAt,
              eventAgents[eventAgents.length - 1] ?? firstResponder,
              "INTERNAL",
              pick(FOLLOW_INT),
            );
          }
        } else {
          emit(lastAt, asker, askerRole, pick(FOLLOW_EXT));
        }
        msgs.sort((a, b) => a.at.localeCompare(b.at));

        const row: EventRow = {
          id,
          corpid: CORPID,
          roomid: room.roomid,
          source_msg_ids: sourceIds,
          first_msg_time: formatDateTime(t0),
          last_msg_time: formatDateTime(lastAt),
          first_agent_reply_time: replyAt ? formatDateTime(replyAt) : null,
          occurred_on: dt,
          asker,
          asker_role: askerRole,
          agents: eventAgents,
          first_responder: firstResponder,
          summary,
          // 与 assemble 同一条规则：排序后的末条。
          last_msg_role: msgs[msgs.length - 1]!.sender_role,
          // mock 不复刻工作时段口径，只造出「有/没有后续轮次」两种形状。
          followup_wait_max_sec: followupWaitMaxSec(msgs),
          event_type: type,
          taxonomy_version: TAXONOMY_VERSION,
        };
        messages.set(id, msgs);
        dayEvents.push(row);
        if (!failed) events.push(row); // 失败的群日在库里根本没有 event 行
      }

      const merchant = failed ? [] : dayEvents.filter((e) => e.asker_role === "EXTERNAL");
      const secs = merchant
        .filter((e) => e.first_agent_reply_time !== null)
        .map(
          (e) =>
            (parseDateTime(e.first_agent_reply_time as string).getTime() -
              parseDateTime(e.first_msg_time).getTime()) /
            1000,
        )
        .sort((a, b) => a - b);
      const q = (p: number) =>
        secs.length
          ? Math.round(secs[Math.min(secs.length - 1, Math.floor(secs.length * p))] as number)
          : null;

      groupDaily.push({
        corpid: CORPID,
        roomid: room.roomid,
        dt,
        msg_count: Math.round((dayEvents.length * rint(30, 55)) / 10) + rint(2, 25),
        sender_count: rint(3, Math.max(4, crew.length + 4)),
        event_count: failed ? null : dayEvents.length,
        merchant_event_count: failed ? null : merchant.length,
        unreplied_count: failed ? null : merchant.length - secs.length,
        first_reply_p50_sec: failed ? null : q(0.5),
        first_reply_p90_sec: failed ? null : q(0.9),
        extraction_status: failed ? "failed" : "ok",
        classification_status: failed ? "failed" : "ok",
      });

      if (failed) {
        failures.push({
          run_date: DAYS[DAYS.length - 1] as string,
          corpid: CORPID,
          roomid: room.roomid,
          dt,
          reason:
            rnd() < 0.5
              ? "抽取批次校验失败：模型返回的 msg_index 越界"
              : "分类打标失败：模型返回了词表中不存在的 type_id",
        });
      } else {
        const bucket = new Map<string, number>();
        for (const e of dayEvents) {
          if (!e.first_responder) continue; // 未回复的事件不落在任何人头上
          const key = `${e.first_responder}|${e.event_type}`;
          bucket.set(key, (bucket.get(key) ?? 0) + 1);
        }
        for (const [key, count] of bucket) {
          const [agent = "", eventType = ""] = key.split("|");
          agentDaily.push({
            corpid: CORPID,
            room: room.roomid,
            agent,
            dt,
            event_type: eventType,
            taxonomy_version: TAXONOMY_VERSION,
            event_count: count,
          });
        }
      }
    }
  }

  return {
    meta: {
      corpid: CORPID,
      days: DAYS,
      rooms: rooms.map(({ roomid, alias }) => ({ roomid, alias })),
      agents: agents.map(({ agent, alias }) => ({ agent, alias })),
      taxonomy: MOCK_TAXONOMY,
      taxonomy_version: TAXONOMY_VERSION,
      // 模拟数据中的群名与客服姓名是占位花名册，界面必须标「待补」。
      alias_is_authoritative: false,
    },
    events,
    groupDaily,
    agentDaily,
    failures,
    messages,
  };
}

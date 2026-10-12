import { beforeEach, expect, it, vi } from "vitest";
import { downloadWorkbook, layoutSheets, type SheetView, type WorkbookSheet } from "./workbook";

// 写文件的库换掉：只验证交给它的入参与文件名。
const toFile = vi.hoisted(() => vi.fn<(name: string) => Promise<void>>(() => Promise.resolve()));
const writeXlsxFile = vi.hoisted(() => vi.fn<(sheets: unknown) => { toFile: typeof toFile }>());
vi.mock("write-excel-file/browser", () => ({ default: writeXlsxFile }));
beforeEach(() => {
  writeXlsxFile.mockReset().mockReturnValue({ toFile });
  toFile.mockClear();
});

const view: SheetView = {
  days: ["2026-10-01", "2026-10-02", "2026-10-03"],
  lastDay: "2026-10-03",
  roomLabel: (id) => `群${id}`,
  agentLabel: (id) => `客服${id}`,
};

const sheets: [WorkbookSheet, WorkbookSheet] = [
  {
    name: "甲",
    caption: "导出条件甲",
    columns: [
      { header: "名称", width: 30 },
      { header: "数量", width: 10 },
    ],
    rows: [
      ["a", 1],
      ["b", null],
    ],
  },
  {
    name: "乙",
    caption: "导出条件乙",
    columns: [{ header: "名称", width: 20 }],
    rows: [],
    stickyColumns: 1,
  },
];

it("lays each sheet out as caption, blank spacer row, bold headers, then the rows", () => {
  expect(layoutSheets(sheets)).toEqual([
    {
      sheet: "甲",
      data: [
        ["导出条件甲"],
        [],
        [
          { value: "名称", fontWeight: "bold" },
          { value: "数量", fontWeight: "bold" },
        ],
        ["a", 1],
        ["b", null],
      ],
      columns: [{ width: 30 }, { width: 10 }],
      stickyRowsCount: 3,
      stickyColumnsCount: 0,
    },
    {
      sheet: "乙",
      data: [["导出条件乙"], [], [{ value: "名称", fontWeight: "bold" }]],
      columns: [{ width: 20 }],
      stickyRowsCount: 3,
      stickyColumnsCount: 1,
    },
  ]);
});

it("downloads every sheet into one file named after the first sheet and the date span", async () => {
  await downloadWorkbook(sheets, view);

  expect(writeXlsxFile).toHaveBeenCalledWith(layoutSheets(sheets));
  expect(toFile).toHaveBeenCalledWith("甲_2026-10-01_2026-10-03.xlsx");
});

it("names the file after the last day on both ends when the window has no days", async () => {
  await downloadWorkbook(sheets, { ...view, days: [] });

  expect(toFile).toHaveBeenCalledWith("甲_2026-10-03_2026-10-03.xlsx");
});

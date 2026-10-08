import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, useLocation, useNavigate } from "react-router-dom";
import { afterEach, beforeAll, describe, expect, it } from "vitest";
import { buildMockDataset } from "@/test/mock/generator";
import { useFilters } from "@/features/filters/useFilters";
import { RoomFilters } from "./RoomFilters";

const { meta } = buildMockDataset();
afterEach(cleanup);

beforeAll(() => {
  window.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
  window.matchMedia = (query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener() {},
    removeEventListener() {},
    addListener() {},
    removeListener() {},
    dispatchEvent: () => false,
  });
});

function Harness() {
  const api = useFilters();
  const location = useLocation();
  const navigate = useNavigate();
  return (
    <>
      <RoomFilters meta={meta} api={api} />
      <output data-testid="url">{location.search}</output>
      <button onClick={() => void navigate(-1)}>返回</button>
    </>
  );
}

function mount(search = "") {
  return render(
    <MemoryRouter initialEntries={[`/rooms${search}`]}>
      <Harness />
    </MemoryRouter>,
  );
}

function params() {
  return new URLSearchParams(screen.getByTestId("url").textContent);
}

describe("群聊分析筛选", () => {
  it("reset_clears_filters_and_legacy_source", async () => {
    mount("?source=api&q=关键词&page=2");
    await userEvent.setup().click(screen.getByRole("button", { name: "重置" }));
    expect(params().toString()).toBe("");
  });
  it("date_presets_update_range_and_preserve_room_and_rank", async () => {
    const user = userEvent.setup();
    const view = mount("?room=R-test&rank=p90&page=3");
    expect(
      [...view.container.querySelectorAll(".ant-segmented-item-label")].map(
        (item) => item.textContent,
      ),
    ).toEqual(["最后 1 天", "近 3 天", "近 7 天"]);
    await user.click(screen.getByText("近 3 天", { exact: true }));
    await waitFor(() => expect(params().get("from")).toBe(meta.days.at(-3)));
    expect(params().get("to")).toBe(meta.days.at(-1));
    expect(params().get("room")).toBe("R-test");
    expect(params().get("rank")).toBe("p90");
    expect(params().has("page")).toBe(false);
    await user.click(screen.getByText("最后 1 天", { exact: true }));
    expect(params().get("from")).toBe(meta.days.at(-1));
    expect(params().get("to")).toBe(meta.days.at(-1));
    await user.click(screen.getByText("近 7 天", { exact: true }));
    expect(params().get("from")).toBe(meta.days[0]);
    view.unmount();
  });

  it("折叠保留有效条件，false 超时条件也计数；修改一级分类清除二级", async () => {
    const user = userEvent.setup();
    const view = mount(`?l2=${encodeURIComponent(meta.taxonomy[0]!.type_id)}&overdue=0`);
    const more = screen.getByRole("button", { name: "更多筛选（2）" });
    expect(more).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("combobox", { name: "一级分类" })).toBeNull();
    await user.click(more);
    await user.click(screen.getByRole("combobox", { name: "一级分类" }));
    await user.click(
      screen.getByText(meta.taxonomy[0]!.parent_name, {
        exact: true,
        selector: ".ant-select-item-option-content",
      }),
    );
    await waitFor(() => expect(params().get("l1")).toBe(meta.taxonomy[0]!.parent_name));
    expect(params().has("l2")).toBe(false);
    expect(params().get("overdue")).toBe("0");
    await user.click(screen.getByRole("button", { name: "更多筛选（2）" }));
    expect(params().get("overdue")).toBe("0");
    view.unmount();
  });

  it("selecting_a_child_category_clears_the_parent_and_preserves_other_filters", async () => {
    const user = userEvent.setup();
    const parent = meta.taxonomy[0]!.parent_name;
    const child = meta.taxonomy.find((type) => type.parent_name !== parent)!;
    mount(`?l1=${encodeURIComponent(parent)}&room=R-test&overdue=1&page=3`);
    await user.click(screen.getByRole("button", { name: "更多筛选（2）" }));
    await user.click(screen.getByRole("combobox", { name: "二级分类" }));
    await user.click(
      screen.getByText(`${child.parent_name} / ${child.name}`, {
        exact: true,
        selector: ".ant-select-item-option-content",
      }),
    );
    expect(params().get("l2")).toBe(child.type_id);
    expect(params().has("l1")).toBe(false);
    expect(params().get("room")).toBe("R-test");
    expect(params().get("overdue")).toBe("1");
    expect(params().has("page")).toBe(false);
  });

  it("关键词保持回车提交，重置与浏览器返回同步输入框", async () => {
    const user = userEvent.setup();
    const view = mount("?q=原关键词&status=unreplied");
    const input = screen.getByLabelText("关键词");
    await user.clear(input);
    await user.type(input, "空调");
    expect(params().get("q")).toBe("原关键词");
    await user.keyboard("{Enter}");
    await waitFor(() => expect(params().get("q")).toBe("空调"));
    expect(params().get("status")).toBe("unreplied");
    await user.click(screen.getByRole("button", { name: "重置" }));
    await waitFor(() => expect(params().toString()).toBe(""));
    expect(input).toHaveValue("");
    expect(screen.getByRole("button", { name: "重置" })).toHaveAttribute("data-active", "false");
    await user.click(screen.getByRole("button", { name: "返回" }));
    await waitFor(() => expect(input).toHaveValue("空调"));
    expect(screen.getByRole("button", { name: "更多筛选（1）" })).toBeInTheDocument();
    view.unmount();
  });

  it("客服选择同时更新 agent 和 focus，保留其他条件", async () => {
    const user = userEvent.setup();
    const view = mount("?overdue=1");
    const agent = meta.agents[0]!;
    await user.click(screen.getByRole("combobox", { name: "客服" }));
    await user.click(screen.getByText(agent.alias ?? agent.agent, { exact: true }));
    await waitFor(() => expect(params().get("agent")).toBe(agent.agent));
    expect(params().get("focus")).toBe(agent.agent);
    expect(params().get("overdue")).toBe("1");
    view.unmount();
  });

  it("搜索按钮提交草稿，清除关键词时保留其他筛选", async () => {
    const user = userEvent.setup();
    const view = mount("?room=R-test&page=3");
    const input = screen.getByLabelText("关键词");
    await user.type(input, "空调");
    expect(params().has("q")).toBe(false);
    await user.click(screen.getByRole("button", { name: "搜索" }));
    await waitFor(() => expect(params().get("q")).toBe("空调"));
    expect(params().get("room")).toBe("R-test");
    expect(params().has("page")).toBe(false);
    await user.click(screen.getByRole("button", { name: "清除" }));
    await waitFor(() => expect(params().has("q")).toBe(false));
    expect(input).toHaveValue("");
    expect(params().get("room")).toBe("R-test");
    view.unmount();
  });

  // 选项从 meta.rooms 去重得出（天然只含当前日期范围内的群）：
  // 「未分组」是上游字面值、照常是一项；没关联商家（NULL）的群不产生选项；
  // 经理姓名缺失（null）或是空串时显示编号。
  it("商家分组与业务经理下拉：选项由群选项去重得出，姓名缺失时回落显示编号", async () => {
    const user = userEvent.setup();
    // 每个下拉用一份新挂载：收起的下拉不会从 DOM 里摘掉，同屏读会把两个下拉的选项混在一起
    const optionsOf = async (name: string) => {
      const view = mount();
      await user.click(screen.getByRole("button", { name: "更多筛选" }));
      await user.click(screen.getByRole("combobox", { name }));
      const texts = [...document.querySelectorAll(".ant-select-item-option-content")].map(
        (el) => el.textContent,
      );
      view.unmount();
      return texts;
    };
    expect(await optionsOf("商家分组")).toEqual(["华北组", "华东组", "华南组", "未分组"]);
    // 同一个经理名下多个群只出一项；姓名 null / 空串的回落成编号
    expect(await optionsOf("业务经理")).toEqual(["1003", "9007199254740993", "李经理", "王经理"]);
  });

  it("选商家分组与业务经理写进 URL，与其他条件叠加并复位页码，重置一并清掉", async () => {
    const user = userEvent.setup();
    const view = mount("?room=R-test&page=3");
    await user.click(screen.getByRole("button", { name: "更多筛选" }));
    await user.click(screen.getByRole("combobox", { name: "商家分组" }));
    await user.click(
      screen.getByText("华东组", { exact: true, selector: ".ant-select-item-option-content" }),
    );
    await waitFor(() => expect(params().get("group")).toBe("华东组"));
    expect(params().get("room")).toBe("R-test");
    expect(params().has("page")).toBe(false);
    await user.click(screen.getByRole("combobox", { name: "业务经理" }));
    await user.click(
      screen.getByText("李经理", { exact: true, selector: ".ant-select-item-option-content" }),
    );
    await waitFor(() => expect(params().get("manager")).toBe("1001"));
    expect(params().get("group")).toBe("华东组");
    expect(screen.getByRole("button", { name: "重置" })).toHaveAttribute("data-active", "true");
    await user.click(screen.getByRole("button", { name: "重置" }));
    await waitFor(() => expect(params().toString()).toBe(""));
    view.unmount();
  });

  it("折叠时两个条件各是一颗可点掉的筹码；经理筹码显示姓名，查不到姓名时显示编号", async () => {
    const user = userEvent.setup();
    const view = mount("?group=未分组&manager=9007199254740993");
    expect(screen.getByRole("button", { name: "更多筛选（2）" })).toHaveAttribute(
      "data-active",
      "true",
    );
    expect(
      screen.getByRole("button", { name: "清除业务经理筛选：9007199254740993" }),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "清除商家分组筛选：未分组" }));
    await waitFor(() => expect(params().has("group")).toBe(false));
    expect(params().get("manager")).toBe("9007199254740993");
    view.unmount();
  });

  it("collapsed_filters_show_removable_chips", async () => {
    const user = userEvent.setup();
    const type = meta.taxonomy[0]!;
    const view = mount(`?l2=${encodeURIComponent(type.type_id)}&overdue=0`);
    expect(screen.getByRole("button", { name: "更多筛选（2）" })).toHaveAttribute(
      "data-active",
      "true",
    );
    expect(
      screen.getByRole("button", { name: `清除二级筛选：${type.parent_name} / ${type.name}` }),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "清除超时筛选：仅未超时" }));
    await waitFor(() => expect(params().has("overdue")).toBe(false));
    expect(params().get("l2")).toBe(type.type_id);
    // 展开后条件在下拉框里看得见，筹码行让位
    await user.click(screen.getByRole("button", { name: "更多筛选（1）" }));
    expect(screen.queryByRole("button", { name: /清除二级筛选/ })).toBeNull();
    view.unmount();
  });
});

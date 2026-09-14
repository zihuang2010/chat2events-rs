import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, expect, it } from "vitest";
import { Metric } from "./Metric";

afterEach(cleanup);

it("names_the_metric_link_with_its_label_value_and_destination_purpose", () => {
  render(
    <MemoryRouter>
      <Metric label="无响应" value="12" unit="起" to="/detail?status=unreplied" />
    </MemoryRouter>,
  );
  expect(screen.getByRole("link", { name: "无响应：12起，查看明细" })).toHaveAttribute(
    "href",
    "/detail?status=unreplied",
  );
  expect(screen.queryByRole("img", { name: "arrow-right" })).not.toBeInTheDocument();
});

it("does_not_expose_a_value_or_drilldown_when_statistics_are_unavailable", () => {
  render(
    <MemoryRouter>
      <Metric label="事件量" value="0" unit="起" to="/detail" unavailable />
    </MemoryRouter>,
  );
  expect(screen.queryByRole("link")).not.toBeInTheDocument();
  expect(screen.getByText("—")).toBeInTheDocument();
  expect(screen.getByText("当前范围事件统计暂缺")).toBeInTheDocument();
  expect(screen.queryByText("0")).not.toBeInTheDocument();
});

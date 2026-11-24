defmodule Geo.Point do
  @moduledoc "A point."
  @origin 0

  defstruct [:x, :y]

  def area(%__MODULE__{x: x}), do: x

  def area(_), do: 0

  defp helper(a) do
    inner = fn -> a end
    inner.()
  end

  defmacro square(x) do
    quote do: unquote(x) * unquote(x)
  end

  defguard is_point(p) when is_map(p)

  defdelegate parse(s), to: Geo.Parser
end

defprotocol Geo.Shape do
  def area(shape)
end

defimpl Geo.Shape, for: Geo.Point do
  def area(p), do: p.x
end

defmodule Geo.Check do
  def valid?(x), do: x != nil
  def run!(x), do: x
end

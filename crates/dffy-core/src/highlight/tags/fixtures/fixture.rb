require_relative "util"

LIMIT = 10

module Geo
  VERSION = "1.0"

  class Point < Base
    attr_reader :x, :y
    ORIGIN = nil

    def initialize(x)
      @x = x
    end

    def self.origin
      new(0)
    end

    def area
      helper = 1
      0
    end

    class << self
      def build; end
    end
  end

  def self.helper; end
end

def top(a)
  a
end

Struct.new(:a)

class Account
  def empty?
    true
  end

  def save!; end

  def name=(value)
    @name = value
  end

  private def secret; end

  memoize def cached
    1
  end
end

module Tools
  module_function def util; end
end

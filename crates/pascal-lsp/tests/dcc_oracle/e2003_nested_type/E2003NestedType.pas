unit E2003NestedType;
interface
implementation
type
  TOuter = class
  public
    type TInner = class end;
  end;
var Good: TOuter.TInner;
    Bad: TOuter.TMissing;
end.

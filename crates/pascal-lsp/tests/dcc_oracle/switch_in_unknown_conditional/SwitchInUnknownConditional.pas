unit SwitchInUnknownConditional;

interface

type
  TObject = class
  end;

  TBase = class(TObject)
  protected
    Value: Integer;
  end;

{$IFDEF SWITCH_MAYBE_DEFINED}
{$WARNINGS OFF}
{$ENDIF}
  TChild = class(TBase)
  private
    FChild: Integer;
  public
    property Value: Integer read FChild;
  end;

implementation

end.

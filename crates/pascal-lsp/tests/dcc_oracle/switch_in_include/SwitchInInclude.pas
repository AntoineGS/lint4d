unit SwitchInInclude;

interface

type
  TObject = class
  end;

  TBase = class(TObject)
  protected
    Value: Integer;
  end;

  TBeforeInclude = class(TBase)
  private
    FBefore: Integer;
  public
    property Value: Integer read FBefore;
  end;

{$I Switches.inc}
  TAfterInclude = class(TBase)
  private
    FAfter: Integer;
  public
    property Value: Integer read FAfter;
  end;

implementation

end.

unit SwitchWarnIdOff;

interface

type
  TObject = class
  end;

  TBase = class(TObject)
  protected
    Value: Integer;
  end;

{$WARN HIDING_MEMBER OFF}
  TChild = class(TBase)
  private
    FChild: Integer;
  public
    property Value: Integer read FChild;
  end;

implementation

end.

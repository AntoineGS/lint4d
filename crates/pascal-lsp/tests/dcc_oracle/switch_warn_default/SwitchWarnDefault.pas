unit SwitchWarnDefault;

interface

type
  TObject = class
  end;

  TBase = class(TObject)
  protected
    Value: Integer;
  end;

{$WARN HIDING_MEMBER OFF}
  TOff = class(TBase)
  private
    FOff: Integer;
  public
    property Value: Integer read FOff;
  end;
{$WARN HIDING_MEMBER DEFAULT}
  TDefault = class(TBase)
  private
    FDefault: Integer;
  public
    property Value: Integer read FDefault;
  end;

implementation

end.

unit SwitchWarningsOffRegion;

interface

type
  TObject = class
  end;

  TBase = class(TObject)
  protected
    Value: Integer;
  end;

{$WARNINGS OFF}
  TOff = class(TBase)
  private
    FOff: Integer;
  public
    property Value: Integer read FOff;
  end;
{$WARNINGS ON}

  TOn = class(TBase)
  private
    FOn: Integer;
  public
    property Value: Integer read FOn;
  end;

implementation

end.
